//! Runtime-owned external call vocabulary for `tina-runtime`.
//!
//! `tina` only owns the [`Effect::Io`](tina::Effect::Io) slot and the
//! `Isolate::Io` associated type. The concrete request and result types
//! live here so the trait crate stays substrate-neutral. A future
//! Mariner runtime crate (a different completion-driven backend, a
//! deterministic simulator, …) can implement the same `Effect::Io`
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

// ---------------------------------------------------------------------------
// Module map (Phase 115 reorg)
// ---------------------------------------------------------------------------
//
// Rail constructors live in per-rail submodules so adding or reading one
// rail does not require scrolling past every other rail. Submodule items
// are re-exported below so existing callers (`tina_runtime::tcp_bind`,
// `tina_runtime::file_open`, ...) are unchanged.
//
//   - `mod tcp`         — `tcp_bind`/`tcp_accept`/`tcp_connect`/read/write/close.
//   - `mod udp`         — `udp_bind`/`udp_send_to`/`udp_recv_from`/close.
//   - `mod tls`         — `tls_connect`/`tls_bind`/`tls_accept`/read/write/close.
//   - `mod dns`         — `dns_lookup`.
//   - `mod signals`     — `signal_wait`.
//   - `mod process`     — `process_run`.
//   - `mod files`       — file + filesystem-path operations.
//   - `mod persistence` — snapshot / journal helpers.
//
//   - `mod io`          — `CallInput`, `CallOutput`, `CallError`,
//     `CallOutcome`, and `SendOutcome`.
//
// This file keeps the type-erased runtime-call machinery (`RuntimeCall*`,
// `ErasedCall`, `IntoErasedCall`) and the `TypedCall*` family because those
// generic builders reference each other heavily. Decoder impls for
// `CallOutput` also stay here next to `TypedCall`.

mod cancel;
mod dns;
mod files;
mod groups;
mod io;
mod pending;
mod persistence;
mod process;
mod signals;
mod tcp;
mod time;
mod tls;
mod types;
mod udp;
mod unix;

pub use cancel::*;
pub use dns::*;
pub use files::*;
pub use groups::*;
pub use io::*;
pub use pending::*;
pub use persistence::*;
pub use process::*;
pub use signals::*;
pub use tcp::*;
pub use time::*;
pub use tls::*;
pub use types::*;
pub use udp::*;
pub use unix::*;

use std::any::Any;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tina::{Address, AddressGeneration, CallHandleShared, CancelOutcome, IsolateId, ShardId};

/// Safe in-runtime wrapper over tina's `unsafe` must-answer-rail escape hatch.
///
/// Every runtime adapter that re-wraps a finished effect into a
/// `RequestEffect` routes through here so the `unsafe` is discharged once,
/// centrally. The hole stays `unsafe` to foreign app crates (which cannot name
/// this `pub(crate)` wrapper); they would have to call the `unsafe`
/// `tina::runtime_internal` form, which the must-answer rail forbids outside an
/// explicit `unsafe` block.
#[allow(unsafe_code)]
pub(crate) fn request_effect_from_consumed_effect<I: tina::Isolate>(
    effect: tina::Effect<I>,
) -> tina::RequestEffect<I> {
    // SAFETY: every caller that reaches this wrapper has already consumed the
    // matching `RequestCall` authority (via `RequestContext`) before producing
    // `effect`, so the manufactured `RequestEffect` settles exactly that one
    // caller — the precondition tina's escape hatch documents.
    unsafe { tina::runtime_internal::request_effect_from_consumed_effect(effect) }
}

type ErasedReply = Box<dyn Any>;
type ErasedCallOutcome = CallOutcome<ErasedReply>;
type IsolateCallTranslator<M> = Box<dyn FnOnce(ErasedCallOutcome) -> M>;
type ErasedIsolateCallTranslator = Box<dyn FnOnce(ErasedCallOutcome) -> Box<dyn Any>>;
type CancelCallTranslator<M> = Box<dyn FnOnce(CancelOutcome) -> M>;
type ErasedCancelCallTranslator = Box<dyn FnOnce(CancelOutcome) -> Box<dyn Any>>;

fn erase_isolate_call_translator<R, M, F>(translator: F) -> IsolateCallTranslator<M>
where
    R: 'static,
    F: FnOnce(CallOutcome<R>) -> M + 'static,
{
    Box::new(move |outcome| match outcome {
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
        CallOutcome::Rejected(reason) => translator(CallOutcome::Rejected(reason)),
    })
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

mod runtime_callable_sealed {
    pub trait Sealed {}
}

/// Marker trait identifying `Isolate::Io` types accepted by simulator-
/// driven and runtime-call-aware contexts.
///
/// This trait is implemented only for [`RuntimeCall`].
/// Surfaces in simulator and runtime-call bounds get a clearer compile
/// error than the previous `Io = RuntimeCall<...>` equality mismatch
/// when an isolate is authored with `#[tina::isolate]` (which defaults
/// `Io = Infallible`).
///
/// `RuntimeCall` satisfies the bound:
///
/// ```
/// use tina_runtime::{RuntimeCall, RuntimeCallable};
/// fn assert_callable<C: RuntimeCallable>() {}
/// assert_callable::<RuntimeCall<u32>>();
/// ```
///
/// `Infallible` (the default `Call` from `#[tina::isolate]`) does not:
///
/// ```compile_fail
/// use tina_runtime::RuntimeCallable;
/// fn assert_callable<C: RuntimeCallable>() {}
/// assert_callable::<std::convert::Infallible>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Tina runtime I/O payload",
    label = "this isolate's `Call` is not `RuntimeCall<...>`",
    note = "`#[tina::isolate(...)]` defaults `Io = std::convert::Infallible`, which simulator-driven and runtime-call-aware paths cannot drive. Switch the attribute to `#[tina_runtime::isolate(...)]`, or supply `io = ::tina_runtime::RuntimeCall<YourMessage>` explicitly."
)]
pub trait RuntimeCallable: runtime_callable_sealed::Sealed {}

impl<M> runtime_callable_sealed::Sealed for RuntimeCall<M> {}
impl<M> RuntimeCallable for RuntimeCall<M> {}

enum RuntimeCallKind<M> {
    Backend {
        request: CallInput,
        translator: Box<dyn FnOnce(CallOutput) -> RuntimeCallCompletion<M>>,
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
        expected_reply_type_id: std::any::TypeId,
        /// Optional caller-owned cancellation cell. Present when the
        /// effect was built via [`call_cancelable`]; the runtime stamps
        /// the assigned `CallId` here on dispatch and updates state on
        /// completion or cancellation.
        handle_shared: Option<Arc<CallHandleShared>>,
    },
    CancelCall {
        handle_shared: Arc<CallHandleShared>,
        translator: CancelCallTranslator<M>,
    },
}

/// What a backend runtime call should do when it completes.
///
/// Most calls produce an ordinary later-turn message. Terminal protocol code
/// may use the narrower actions to avoid a handler turn when no user policy
/// boundary is crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCallCompletion<M> {
    /// Deliver the message to the requesting isolate, which is the ordinary
    /// Tina completion path.
    Message(M),
    /// Stop the requesting isolate after a successful backend completion.
    StopRequester,
    /// Record the successful backend completion and enqueue no message.
    Noop,
}

#[doc(hidden)]
pub enum ErasedRuntimeCallCompletion {
    Message(Box<dyn Any>),
    StopRequester,
    Noop,
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
        Self::new_with_completion(request, move |output| {
            RuntimeCallCompletion::Message(translator(output))
        })
    }

    /// Creates a runtime-owned call request with a terminal completion action.
    ///
    /// This is deliberately narrower than "run another effect". Use
    /// [`RuntimeCallCompletion::Message`] for normal user-visible continuations.
    /// `StopRequester` and `Noop` are for protocol-internal terminal work after
    /// a successful backend completion; backend failures still use the ordinary
    /// message path so failure truth cannot disappear.
    pub fn new_with_completion<F>(request: CallInput, translator: F) -> Self
    where
        F: FnOnce(CallOutput) -> RuntimeCallCompletion<M> + 'static,
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
    pub fn observed_send<T, R, F>(destination: Address<T, R>, message: T, translator: F) -> Self
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
    ///
    /// Same-shard calls complete inside one shard runtime. Live local
    /// multi-shard systems route cross-shard requests and replies through
    /// bounded shard-pair paths.
    ///
    /// Keep `translator` pure: the runtime may run it even when the resulting
    /// message cannot be delivered because the requester stopped or its
    /// mailbox filled.
    pub fn isolate_call<T, R, F>(
        destination: Address<T, R>,
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
                translator: erase_isolate_call_translator::<R, _, _>(translator),
                expected_reply_type_id: std::any::TypeId::of::<R>(),
                handle_shared: None,
            },
        }
    }

    /// Like [`Self::isolate_call`] but carries a caller-owned shared
    /// cell for cancellation. The runtime stamps the assigned `CallId`
    /// on `handle_shared` at dispatch time.
    pub fn isolate_call_with_handle<T, R, F>(
        destination: Address<T, R>,
        message: T,
        timeout: Duration,
        translator: F,
        handle_shared: Arc<CallHandleShared>,
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
                translator: erase_isolate_call_translator::<R, _, _>(translator),
                expected_reply_type_id: std::any::TypeId::of::<R>(),
                handle_shared: Some(handle_shared),
            },
        }
    }

    /// Creates a cancel-call request that closes one pending isolate
    /// call's caller-side wait.
    pub fn cancel_call_with_handle<F>(handle_shared: Arc<CallHandleShared>, translator: F) -> Self
    where
        F: FnOnce(CancelOutcome) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::CancelCall {
                handle_shared,
                translator: Box::new(translator),
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
            CallOutput::TcpReadBufFailed { error, .. }
            | CallOutput::TcpWroteOwnedFailed { error, .. }
            | CallOutput::TlsReadBufFailed { error, .. }
            | CallOutput::TlsWroteOwnedFailed { error, .. } => translator(Err(error)),
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
            RuntimeCallKind::CancelCall { .. } => {
                panic!("cancel call does not carry a backend CallInput")
            }
        }
    }

    /// Splits the call into its request and message translator.
    ///
    /// This compatibility helper is only for ordinary message-producing
    /// calls. Use [`Self::into_completion_parts`] when a backend or test
    /// harness must preserve terminal completion actions.
    ///
    /// # Panics
    ///
    /// Panics when the call is not a backend request, or when the completion
    /// translator returns [`RuntimeCallCompletion::StopRequester`] or
    /// [`RuntimeCallCompletion::Noop`].
    pub fn into_parts(self) -> (CallInput, Box<dyn FnOnce(CallOutput) -> M>)
    where
        M: 'static,
    {
        let (request, translator) = self.into_completion_parts();
        (
            request,
            Box::new(move |output| match translator(output) {
                RuntimeCallCompletion::Message(message) => message,
                RuntimeCallCompletion::StopRequester | RuntimeCallCompletion::Noop => {
                    panic!("RuntimeCall::into_parts cannot unwrap a terminal completion action")
                }
            }),
        )
    }

    /// Splits the call into its request and full completion translator.
    ///
    /// Runtime backends and tests that can see terminal actions should prefer
    /// this over [`Self::into_parts`]. It preserves
    /// [`RuntimeCallCompletion::StopRequester`] and
    /// [`RuntimeCallCompletion::Noop`] instead of forcing them through the
    /// ordinary message path.
    ///
    /// # Panics
    ///
    /// Panics when the call is not a backend request.
    pub fn into_completion_parts(
        self,
    ) -> (
        CallInput,
        Box<dyn FnOnce(CallOutput) -> RuntimeCallCompletion<M>>,
    ) {
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
            RuntimeCallKind::CancelCall { .. } => {
                panic!("cancel call does not carry backend call parts")
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
                expected_reply_type_id,
                handle_shared,
            } => RuntimeCallParts::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
                expected_reply_type_id,
                handle_shared,
            },
            RuntimeCallKind::CancelCall {
                handle_shared,
                translator,
            } => RuntimeCallParts::CancelCall {
                handle_shared,
                translator,
            },
        }
    }
}

/// Publicly destructurable runtime action carried by [`RuntimeCall`].
///
/// `IsolateCall::handle_shared` and the `CancelCall` variant carry the
/// caller-owned cancellation cell so sibling runtime crates (`tina-sim`,
/// future deterministic backends) can implement the same cancel
/// semantics without reaching into private fields.
#[doc(hidden)]
pub enum RuntimeCallParts<M> {
    /// Backend-owned I/O/time request.
    Backend {
        /// Concrete backend request.
        request: CallInput,
        /// Completion translator.
        translator: Box<dyn FnOnce(CallOutput) -> RuntimeCallCompletion<M>>,
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
        /// `TypeId::of::<R>()` for the dispatching `Address<_, R>`.
        expected_reply_type_id: std::any::TypeId,
        /// Optional caller-owned cancellation cell. Set by
        /// [`call_cancelable`].
        handle_shared: Option<Arc<CallHandleShared>>,
    },
    /// Cancel-call request: close one pending isolate call's wait.
    CancelCall {
        /// Caller-owned shared cell identifying the call.
        handle_shared: Arc<CallHandleShared>,
        /// Outcome translator.
        translator: CancelCallTranslator<M>,
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
                    RuntimeCallKind::CancelCall { .. } => crate::trace::CallKind::CancelCall,
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
        translator: Box<dyn FnOnce(CallOutput) -> ErasedRuntimeCallCompletion>,
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
        /// `TypeId::of::<R>()` for the dispatching `Address<_, R>`.
        /// Used to typecheck deferred-reply payloads before they
        /// reach the translator's downcast.
        expected_reply_type_id: std::any::TypeId,
        /// Optional caller-owned cancellation cell.
        handle_shared: Option<Arc<CallHandleShared>>,
    },
    /// Cancel-call request: close one pending isolate call's wait.
    CancelCall {
        /// Caller-owned shared cell identifying the call.
        handle_shared: Arc<CallHandleShared>,
        /// Outcome translator erased to `Any`.
        translator: ErasedCancelCallTranslator,
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
                    ErasedCallKind::CancelCall { .. } => crate::trace::CallKind::CancelCall,
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
                    translator: Box::new(move |result| match translator(result) {
                        RuntimeCallCompletion::Message(message) => {
                            ErasedRuntimeCallCompletion::Message(Box::new(message))
                        }
                        RuntimeCallCompletion::StopRequester => {
                            ErasedRuntimeCallCompletion::StopRequester
                        }
                        RuntimeCallCompletion::Noop => ErasedRuntimeCallCompletion::Noop,
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
                expected_reply_type_id,
                handle_shared,
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
                    expected_reply_type_id,
                    handle_shared,
                },
            },
            RuntimeCallParts::CancelCall {
                handle_shared,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::CancelCall {
                    handle_shared,
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

    /// Extracts the successful TCP connect result.
    pub fn into_tcp_connected(self) -> Result<(StreamId, SocketAddr, SocketAddr), CallError> {
        match self {
            Self::TcpConnected {
                stream,
                local_addr,
                peer_addr,
            } => Ok((stream, local_addr, peer_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpConnected", &other),
        }
    }

    /// Extracts the successful TCP read payload.
    pub fn into_tcp_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::TcpRead { bytes } => Ok(bytes),
            Self::TcpReadBuf { buffer, len } => Ok(buffer.into_iter().take(len).collect()),
            Self::TcpReadBufFailed { error, .. } => Err(error),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpRead", &other),
        }
    }

    /// Extracts the successful TCP reusable-buffer read payload.
    pub fn into_tcp_read_buf(self) -> Result<TcpReadBufReply, TcpReadBufError> {
        match self {
            Self::TcpReadBuf { buffer, len } => Ok(TcpReadBufReply { buffer, len }),
            Self::TcpRead { bytes } => {
                let len = bytes.len();
                Ok(TcpReadBufReply { buffer: bytes, len })
            }
            Self::TcpReadBufFailed { buffer, error } => Err(TcpReadBufError { error, buffer }),
            Self::Failed(error) => Err(TcpReadBufError {
                error,
                buffer: Vec::new(),
            }),
            other => Self::panic_wrong_shape("TcpReadBuf", &other),
        }
    }

    /// Extracts the successful TCP write payload.
    pub fn into_tcp_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::TcpWrote { count } => Ok(count),
            Self::TcpWroteOwned { count, .. } => Ok(count),
            Self::TcpWroteOwnedClose { count, .. } => Ok(count),
            Self::TcpWroteOwnedFailed { error, .. } => Err(error),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpWrote", &other),
        }
    }

    /// Extracts the successful TCP reusable-buffer write payload.
    pub fn into_tcp_wrote_owned(self) -> Result<TcpWriteOwnedReply, TcpWriteOwnedError> {
        match self {
            Self::TcpWroteOwned { bytes, count } => Ok(TcpWriteOwnedReply {
                bytes,
                written: count,
            }),
            Self::TcpWroteOwnedFailed { bytes, error } => Err(TcpWriteOwnedError { error, bytes }),
            Self::Failed(error) => Err(TcpWriteOwnedError {
                error,
                bytes: Vec::new(),
            }),
            other => Self::panic_wrong_shape("TcpWroteOwned", &other),
        }
    }

    /// Extracts the successful TCP reusable-buffer terminal-write payload.
    pub fn into_tcp_wrote_owned_close(self) -> Result<TcpWriteOwnedCloseReply, TcpWriteOwnedError> {
        match self {
            Self::TcpWroteOwnedClose {
                bytes,
                count,
                closed,
            } => Ok(TcpWriteOwnedCloseReply {
                bytes,
                written: count,
                closed,
            }),
            Self::TcpWroteOwned { bytes, count } => Ok(TcpWriteOwnedCloseReply {
                bytes,
                written: count,
                closed: false,
            }),
            Self::TcpWroteOwnedFailed { bytes, error } => Err(TcpWriteOwnedError { error, bytes }),
            Self::Failed(error) => Err(TcpWriteOwnedError {
                error,
                bytes: Vec::new(),
            }),
            other => Self::panic_wrong_shape("TcpWroteOwnedClose", &other),
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

    /// Extracts the successful UDP bind result.
    pub fn into_udp_bound(self) -> Result<(UdpSocketId, SocketAddr), CallError> {
        match self {
            Self::UdpBound { socket, local_addr } => Ok((socket, local_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpBound", &other),
        }
    }

    /// Extracts the successful UDP send count.
    pub fn into_udp_sent(self) -> Result<usize, CallError> {
        match self {
            Self::UdpSent { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpSent", &other),
        }
    }

    /// Extracts the successful UDP receive payload.
    pub fn into_udp_received(self) -> Result<(SocketAddr, Vec<u8>, bool), CallError> {
        match self {
            Self::UdpReceived {
                peer_addr,
                bytes,
                truncated,
            } => Ok((peer_addr, bytes, truncated)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpReceived", &other),
        }
    }

    /// Extracts the successful UDP socket close completion.
    pub fn into_udp_socket_closed(self) -> Result<(), CallError> {
        match self {
            Self::UdpSocketClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UdpSocketClosed", &other),
        }
    }

    /// Extracts the successful TLS connect result (stream id only,
    /// discarding any negotiated ALPN). Used by non-ALPN callers.
    pub fn into_tls_connected(self) -> Result<TlsStreamId, CallError> {
        match self {
            Self::TlsConnected { stream, .. } => Ok(stream),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsConnected", &other),
        }
    }

    /// Extracts the TLS connect result plus the negotiated ALPN protocol
    /// (raw wire bytes), or `None` when no ALPN was negotiated.
    pub fn into_tls_connected_alpn(self) -> Result<(TlsStreamId, Option<Vec<u8>>), CallError> {
        match self {
            Self::TlsConnected {
                stream,
                selected_alpn,
            } => Ok((stream, selected_alpn)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsConnected", &other),
        }
    }

    /// Extracts the successful TLS bind result.
    pub fn into_tls_bound(self) -> Result<(TlsListenerId, SocketAddr), CallError> {
        match self {
            Self::TlsBound {
                listener,
                local_addr,
            } => Ok((listener, local_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsBound", &other),
        }
    }

    /// Extracts the successful TLS accept result (stream + peer address,
    /// discarding any negotiated ALPN).
    pub fn into_tls_accepted(self) -> Result<(TlsStreamId, SocketAddr), CallError> {
        match self {
            Self::TlsAccepted {
                stream, peer_addr, ..
            } => Ok((stream, peer_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsAccepted", &other),
        }
    }

    /// Extracts the TLS accept result plus the negotiated ALPN protocol.
    pub fn into_tls_accepted_alpn(
        self,
    ) -> Result<(TlsStreamId, SocketAddr, Option<Vec<u8>>), CallError> {
        match self {
            Self::TlsAccepted {
                stream,
                peer_addr,
                selected_alpn,
            } => Ok((stream, peer_addr, selected_alpn)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsAccepted", &other),
        }
    }

    /// Extracts the successful TLS read payload.
    pub fn into_tls_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::TlsRead { bytes } => Ok(bytes),
            Self::TlsReadBuf { buffer, len } => Ok(buffer.into_iter().take(len).collect()),
            Self::TlsReadBufFailed { error, .. } => Err(error),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsRead", &other),
        }
    }

    /// Extracts the successful TLS reusable-buffer read payload.
    pub fn into_tls_read_buf(self) -> Result<TlsReadBufReply, TlsReadBufError> {
        match self {
            Self::TlsReadBuf { buffer, len } => Ok(TlsReadBufReply { buffer, len }),
            Self::TlsRead { bytes } => {
                let len = bytes.len();
                Ok(TlsReadBufReply { buffer: bytes, len })
            }
            Self::TlsReadBufFailed { buffer, error } => Err(TlsReadBufError { error, buffer }),
            Self::Failed(error) => Err(TlsReadBufError {
                error,
                buffer: Vec::new(),
            }),
            other => Self::panic_wrong_shape("TlsReadBuf", &other),
        }
    }

    /// Extracts the successful TLS write count.
    pub fn into_tls_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::TlsWrote { count } => Ok(count),
            Self::TlsWroteOwned { count, .. } => Ok(count),
            Self::TlsWroteOwnedFailed { error, .. } => Err(error),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsWrote", &other),
        }
    }

    /// Extracts the successful TLS reusable-buffer write payload.
    pub fn into_tls_wrote_owned(self) -> Result<TlsWriteOwnedReply, TlsWriteOwnedError> {
        match self {
            Self::TlsWroteOwned { bytes, count } => Ok(TlsWriteOwnedReply {
                bytes,
                written: count,
            }),
            Self::TlsWroteOwnedFailed { bytes, error } => Err(TlsWriteOwnedError { error, bytes }),
            Self::Failed(error) => Err(TlsWriteOwnedError {
                error,
                bytes: Vec::new(),
            }),
            other => Self::panic_wrong_shape("TlsWroteOwned", &other),
        }
    }

    /// Extracts the successful TLS close completion.
    pub fn into_tls_closed(self) -> Result<(), CallError> {
        match self {
            Self::TlsClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsClosed", &other),
        }
    }

    /// Extracts the successful TLS listener close completion.
    pub fn into_tls_listener_closed(self) -> Result<(), CallError> {
        match self {
            Self::TlsListenerClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TlsListenerClosed", &other),
        }
    }

    /// Extracts the successful DNS lookup result.
    pub fn into_dns_resolved(self) -> Result<Vec<SocketAddr>, CallError> {
        match self {
            Self::DnsResolved { addrs } => Ok(addrs),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("DnsResolved", &other),
        }
    }

    /// Extracts the successful signal-wait result.
    pub fn into_signal_received(self) -> Result<String, CallError> {
        match self {
            Self::SignalReceived { name } => Ok(name),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("SignalReceived", &other),
        }
    }

    /// Extracts the successful bounded process result.
    pub fn into_process_exited(self) -> Result<ProcessRunResult, CallError> {
        match self {
            Self::ProcessExited {
                status,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            } => Ok(ProcessRunResult {
                status,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            }),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("ProcessExited", &other),
        }
    }

    /// Extracts the successful file open result.
    pub fn into_file_opened(self) -> Result<FileId, CallError> {
        match self {
            Self::FileOpened { file } => Ok(file),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileOpened", &other),
        }
    }

    /// Extracts the successful file read payload.
    pub fn into_file_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::FileRead { bytes } => Ok(bytes),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileRead", &other),
        }
    }

    /// Extracts the successful file write count.
    pub fn into_file_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::FileWrote { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileWrote", &other),
        }
    }

    /// Extracts the successful file fsync result.
    pub fn into_file_synced(self) -> Result<(), CallError> {
        match self {
            Self::FileSynced => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileSynced", &other),
        }
    }

    /// Extracts the successful file size result.
    pub fn into_file_size(self) -> Result<u64, CallError> {
        match self {
            Self::FileSize { size } => Ok(size),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileSize", &other),
        }
    }

    /// Extracts the successful file close result.
    pub fn into_file_closed(self) -> Result<(), CallError> {
        match self {
            Self::FileClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileClosed", &other),
        }
    }

    /// Extracts the successful mkdir result.
    pub fn into_directory_created(self) -> Result<(), CallError> {
        match self {
            Self::DirectoryCreated => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("DirectoryCreated", &other),
        }
    }

    /// Extracts the successful path metadata result.
    pub fn into_path_metadata(self) -> Result<PathMetadata, CallError> {
        match self {
            Self::PathMetadata { metadata } => Ok(metadata),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("PathMetadata", &other),
        }
    }

    /// Extracts the successful rename/replace result.
    pub fn into_path_renamed(self) -> Result<(), CallError> {
        match self {
            Self::PathRenamed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("PathRenamed", &other),
        }
    }

    /// Extracts the successful remove-file result.
    pub fn into_file_removed(self) -> Result<(), CallError> {
        match self {
            Self::FileRemoved => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("FileRemoved", &other),
        }
    }

    /// Extracts the successful read-dir result.
    pub fn into_directory_read(self) -> Result<Vec<PathBuf>, CallError> {
        match self {
            Self::DirectoryRead { entries } => Ok(entries),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("DirectoryRead", &other),
        }
    }

    /// Extracts the successful parent-directory sync result.
    pub fn into_parent_synced(self) -> Result<(), CallError> {
        match self {
            Self::ParentSynced => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("ParentSynced", &other),
        }
    }

    /// Extracts the successful Unix bind result.
    pub fn into_unix_bound(self) -> Result<(UnixListenerId, std::path::PathBuf), CallError> {
        match self {
            Self::UnixBound { listener, path } => Ok((listener, path)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixBound", &other),
        }
    }

    /// Extracts the successful Unix accept result.
    pub fn into_unix_accepted(self) -> Result<UnixStreamId, CallError> {
        match self {
            Self::UnixAccepted { stream } => Ok(stream),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixAccepted", &other),
        }
    }

    /// Extracts the successful Unix connect result.
    pub fn into_unix_connected(self) -> Result<UnixStreamId, CallError> {
        match self {
            Self::UnixConnected { stream } => Ok(stream),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixConnected", &other),
        }
    }

    /// Extracts the successful Unix read payload.
    pub fn into_unix_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::UnixRead { bytes } => Ok(bytes),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixRead", &other),
        }
    }

    /// Extracts the successful Unix write count.
    pub fn into_unix_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::UnixWrote { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixWrote", &other),
        }
    }

    /// Extracts the successful Unix listener close completion.
    pub fn into_unix_listener_closed(self) -> Result<(), CallError> {
        match self {
            Self::UnixListenerClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixListenerClosed", &other),
        }
    }

    /// Extracts the successful Unix stream close completion.
    pub fn into_unix_stream_closed(self) -> Result<(), CallError> {
        match self {
            Self::UnixStreamClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("UnixStreamClosed", &other),
        }
    }

    /// Extracts the successful snapshot commit result.
    pub fn into_snapshot_committed(self) -> Result<(), CallError> {
        match self {
            Self::SnapshotCommitted => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("SnapshotCommitted", &other),
        }
    }

    /// Extracts the successful snapshot load result.
    pub fn into_snapshot_loaded(self) -> Result<Option<SnapshotImage>, CallError> {
        match self {
            Self::SnapshotLoaded { snapshot } => Ok(snapshot),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("SnapshotLoaded", &other),
        }
    }

    /// Extracts the successful journal append result.
    pub fn into_journal_appended(self) -> Result<(), CallError> {
        match self {
            Self::JournalAppended { .. } => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("JournalAppended", &other),
        }
    }

    /// Extracts the successful journal replay result.
    pub fn into_journal_replayed(self) -> Result<JournalReplay, CallError> {
        match self {
            Self::JournalReplayed { replay } => Ok(replay),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("JournalReplayed", &other),
        }
    }
}

/// Doc-hidden carrier used by typed call helpers like [`sleep`] and
/// [`tcp_read`].
#[doc(hidden)]
pub struct TypedCall<T, E = CallError> {
    request: CallInput,
    decode: fn(CallOutput) -> Result<T, E>,
}

/// Prepared observed-send helper returned by [`send_observed`].
#[doc(hidden)]
pub struct ObservedSend<T> {
    destination: Address<T>,
    message: T,
}

/// Prepared observed-send continuation after caller authority was captured.
#[doc(hidden)]
pub struct DeferredObservedSend<T, Q> {
    inner: ObservedSend<T>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredObservedSend`].
#[doc(hidden)]
pub struct RequestDeferredObservedSend<T, Q> {
    inner: DeferredObservedSend<T, Q>,
}

/// Prepared isolate-call helper returned by [`call`].
#[doc(hidden)]
pub struct IsolateCall<T, R> {
    destination: Address<T, R>,
    message: T,
    timeout: Duration,
    marker: std::marker::PhantomData<fn() -> R>,
}

/// Prepared isolate-call continuation after caller authority was captured.
#[doc(hidden)]
pub struct DeferredIsolateCall<T, R, Q> {
    inner: IsolateCall<T, R>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredIsolateCall`].
#[doc(hidden)]
pub struct RequestDeferredIsolateCall<T, R, Q> {
    inner: DeferredIsolateCall<T, R, Q>,
}

/// Prepared typed runtime-call continuation after caller authority was
/// captured.
#[doc(hidden)]
pub struct DeferredTypedCall<T, Q, E = CallError> {
    inner: TypedCall<T, E>,
    request: tina::RequestContext<Q>,
}

/// Request-effect wrapper for [`DeferredTypedCall`].
#[doc(hidden)]
pub struct RequestDeferredTypedCall<T, Q, E = CallError> {
    inner: DeferredTypedCall<T, Q, E>,
}

impl<T> ObservedSend<T>
where
    T: Send + 'static,
{
    fn new<R>(destination: Address<T, R>, message: T) -> Self {
        Self {
            destination: destination.with_reply::<()>(),
            message,
        }
    }

    /// Turns this prepared observed send into one ordinary later message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(SendOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turns this prepared observed send into one ordinary later message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(SendOutcome) -> M + 'static,
        M: 'static,
    {
        tina::Effect::Io(RuntimeCall::observed_send(
            self.destination,
            self.message,
            translator,
        ))
    }

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message so a multi-turn
    /// service can still answer the original caller after the observed
    /// send resolves.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then_with_request(...)`; use `call_ctx.defer(work).reply(...)` when starting from CallContext"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request(req, translator)
    }

    /// Alias for [`reply_with_request`](Self::reply_with_request) using the
    /// ordinary-continuation vocabulary.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        tina::Effect::Io(RuntimeCall::observed_send(
            self.destination,
            self.message,
            move |outcome| translator(req, outcome),
        ))
    }
}

impl<T, Q> DeferredObservedSend<T, Q>
where
    T: Send + 'static,
    Q: 'static,
{
    /// Builds the continuation that carries the captured caller request.
    ///
    /// This does not reply to the caller by itself. The generated message must
    /// later consume the [`RequestContext`](tina::RequestContext) with
    /// `reply_to_request`.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner.then_with_request(self.request, translator)
    }
}

impl<T, Q> RequestDeferredObservedSend<T, Q>
where
    T: Send + 'static,
    Q: 'static,
{
    /// Builds a request effect whose continuation carries caller authority.
    pub fn reply<I, F, M>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, SendOutcome) -> M + 'static,
        M: 'static,
    {
        crate::call::request_effect_from_consumed_effect(self.inner.reply(translator))
    }
}

impl<T, I> tina::DeferThrough<I> for ObservedSend<T>
where
    T: Send + 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredObservedSend<T, I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        DeferredObservedSend {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, I> tina::RequestDeferThrough<I> for ObservedSend<T>
where
    T: Send + 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred = RequestDeferredObservedSend<T, I::Reply>;

    fn defer_request_through(self, call: tina::RequestCall<'_, I>) -> Self::RequestDeferred {
        RequestDeferredObservedSend {
            inner: <Self as tina::DeferThrough<I>>::defer_through(self, call.into_call_context()),
        }
    }
}

/// Returns a helper that attempts one send and later reports its outcome.
///
/// This is the overload-aware companion to ordinary fire-and-forget
/// [`tina::send`]. For cross-shard sends, [`SendOutcome::Accepted`] means the
/// source shard accepted the message into bounded transport toward the target
/// shard; destination-local mailbox failure is still recorded on the
/// destination trace.
pub fn send_observed<T, R>(destination: Address<T, R>, message: T) -> ObservedSend<T>
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
    fn new(destination: Address<T, R>, message: T, timeout: Duration) -> Self {
        Self {
            destination,
            message,
            timeout,
            marker: std::marker::PhantomData,
        }
    }

    /// Turns this prepared call into one ordinary later message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turns this prepared call into one ordinary later message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        tina::Effect::Io(RuntimeCall::isolate_call(
            self.destination,
            self.message,
            self.timeout,
            translator,
        ))
    }

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message so a multi-turn
    /// service can still answer the original caller after the child call
    /// resolves.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then_with_request(...)`; use `call_ctx.defer(work).reply(...)` when starting from CallContext"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request(req, translator)
    }

    /// Alias for [`reply_with_request`](Self::reply_with_request) using the
    /// ordinary-continuation vocabulary.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        tina::Effect::Io(RuntimeCall::isolate_call(
            self.destination,
            self.message,
            self.timeout,
            move |outcome| translator(req, outcome),
        ))
    }
}

impl<T, R, Q> DeferredIsolateCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds the continuation that carries the captured caller request.
    ///
    /// This does not reply to the caller by itself. The generated message must
    /// later consume the [`RequestContext`](tina::RequestContext) with
    /// `reply_to_request`.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        self.inner.then_with_request(self.request, translator)
    }
}

impl<T, R, Q> RequestDeferredIsolateCall<T, R, Q>
where
    T: Send + 'static,
    R: 'static,
    Q: 'static,
{
    /// Builds a request effect whose continuation carries caller authority.
    pub fn reply<I, F, M>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        crate::call::request_effect_from_consumed_effect(self.inner.reply(translator))
    }
}

impl<T, R, I> tina::DeferThrough<I> for IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredIsolateCall<T, R, I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        DeferredIsolateCall {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, R, I> tina::RequestDeferThrough<I> for IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred = RequestDeferredIsolateCall<T, R, I::Reply>;

    fn defer_request_through(self, call: tina::RequestCall<'_, I>) -> Self::RequestDeferred {
        RequestDeferredIsolateCall {
            inner: <Self as tina::DeferThrough<I>>::defer_through(self, call.into_call_context()),
        }
    }
}

/// Returns a helper that calls another isolate and requires a timeout.
///
/// Same-shard calls complete inside one shard runtime. Live local multi-shard
/// systems route cross-shard requests and replies through bounded shard-pair
/// paths. Keep the `.then(...)` translator pure: it may run even when the
/// translated message is later rejected by the requester's mailbox.
///
/// ```compile_fail
/// use std::time::Duration;
///
/// use tina::{Address, IsolateId, ShardId};
/// use tina_runtime::{call, CallOutcome};
///
/// enum Request {
///     Ask,
/// }
/// struct CorrectReply;
/// struct WrongReply;
///
/// let target: Address<Request, CorrectReply> =
///     Address::new_with_generation(ShardId::new(0), IsolateId::new(1), tina::AddressGeneration::new(0));
/// let _bad = call::<Request, WrongReply>(target, Request::Ask, Duration::from_millis(1));
/// ```
pub fn call<T, R>(destination: Address<T, R>, message: T, timeout: Duration) -> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    IsolateCall::new(destination, message, timeout)
}

/// Capability-typed call that accepts only a [`tina::CallAddress`].
///
/// `call_typed(call_address, msg, timeout)` is the preferred call entry point.
/// Passing a [`SendAddress`](tina::SendAddress) or the `.send` lane of a
/// [`ServiceHandle`](crate::ServiceHandle) is a compile error. The runtime
/// semantics are identical to [`call`]; the only difference is the boundary
/// type-check.
///
/// Negative fixture: calling a send-only address is a compile error.
///
/// ```compile_fail
/// use std::time::Duration;
/// use tina::{Address, AddressGeneration, IsolateId, SendAddress, ShardId};
/// use tina_runtime::call_typed;
///
/// enum Msg { Tick }
/// struct Reply;
///
/// let raw: Address<Msg, Reply> = Address::new_with_generation(
///     ShardId::new(0),
///     IsolateId::new(1),
///     AddressGeneration::new(0),
/// );
/// let send_only: SendAddress<Msg> = raw.send_only();
/// // Expected `CallAddress`, found `SendAddress`.
/// let _ = call_typed(send_only, Msg::Tick, Duration::from_millis(1));
/// ```
pub fn call_typed<T, R>(
    destination: tina::CallAddress<T, R>,
    message: T,
    timeout: Duration,
) -> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    IsolateCall::new(destination.address(), message, timeout)
}

/// Capability-typed call for split service requests.
///
/// This is the request-lane companion to [`tina::send_event`]. Passing an
/// event address here is a compile error, and request payloads are wrapped
/// into [`tina::ServiceMessage::Request`] before dispatch.
pub fn call_request<E, Q, R>(
    destination: tina::ServiceRequestAddress<E, Q, R>,
    request: Q,
    timeout: Duration,
) -> IsolateCall<tina::ServiceMessage<E, Q>, R>
where
    E: Send + 'static,
    Q: Send + 'static,
    R: 'static,
{
    IsolateCall::new(
        destination.address().address(),
        tina::ServiceMessage::Request(request),
        timeout,
    )
}

// ---------------------------------------------------------------------------
// Type aliases for runtime-call replies.
//
// Lets isolate enums spell `Connected(TcpConnectReply)` instead of
// `Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>)` and
// keeps the concrete payload visible by way of the alias name.
// ---------------------------------------------------------------------------

/// The shape every runtime-owned call delivers back: success payload or
/// typed [`CallError`].
pub type CallReply<T> = Result<T, CallError>;

/// Reply delivered by [`sleep`].
pub type SleepReply = CallReply<()>;

/// Reply delivered by [`tcp_bind`].
pub type TcpBindReply = CallReply<(ListenerId, SocketAddr)>;

/// Reply delivered by [`tcp_accept`].
pub type TcpAcceptReply = CallReply<(StreamId, SocketAddr)>;

/// Reply delivered by [`tcp_connect`].
pub type TcpConnectReply = CallReply<(StreamId, SocketAddr, SocketAddr)>;

/// Reply delivered by [`tcp_read`].
pub type TcpReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`tcp_write`].
pub type TcpWriteReply = CallReply<usize>;

/// Reply delivered by [`tcp_close_listener`].
pub type TcpListenerCloseReply = CallReply<()>;

/// Reply delivered by [`tcp_close_stream`].
pub type TcpStreamCloseReply = CallReply<()>;

/// Reply delivered by [`udp_bind`].
pub type UdpBindReply = CallReply<(UdpSocketId, SocketAddr)>;

/// Reply delivered by [`udp_send_to`].
pub type UdpSendToReply = CallReply<usize>;

/// Reply delivered by [`udp_recv_from`]. The `bool` is `true` on truncated
/// datagrams.
pub type UdpRecvFromReply = CallReply<(SocketAddr, Vec<u8>, bool)>;

/// Reply delivered by [`udp_close_socket`].
pub type UdpCloseSocketReply = CallReply<()>;

/// Reply delivered by [`tls_connect`].
pub type TlsConnectReply = CallReply<TlsStreamId>;

/// Reply delivered by [`tls_bind`].
pub type TlsBindReply = CallReply<(TlsListenerId, SocketAddr)>;

/// Reply delivered by [`tls_accept`].
pub type TlsAcceptReply = CallReply<(TlsStreamId, SocketAddr)>;

/// Reply delivered by [`tls_close_listener`].
pub type TlsListenerCloseReply = CallReply<()>;

/// Reply delivered by [`tls_read`].
pub type TlsReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`tls_write`].
pub type TlsWriteReply = CallReply<usize>;

/// Reply delivered by [`tls_close`].
pub type TlsCloseReply = CallReply<()>;

/// Reply delivered by [`snapshot_commit`].
pub type SnapshotCommitReply = CallReply<()>;

/// Reply delivered by [`snapshot_load`].
pub type SnapshotLoadReply = CallReply<Option<SnapshotImage>>;

/// Reply delivered by [`journal_append`].
pub type JournalAppendReply = CallReply<()>;

/// Reply delivered by [`journal_replay`].
pub type JournalReplayReply = CallReply<JournalReplay>;

/// Reply delivered by [`signal_wait`].
pub type SignalWaitReply = CallReply<String>;

/// Reply delivered by [`dns_lookup`].
pub type DnsLookupReply = CallReply<Vec<SocketAddr>>;

/// Reply delivered by [`process_run`].
pub type ProcessRunReply = CallReply<ProcessRunResult>;

/// Reply delivered by [`file_open`] / [`file_create`].
pub type FileOpenReply = CallReply<FileId>;

/// Reply delivered by [`file_read`] / [`file_read_at`].
pub type FileReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`file_write`] / [`file_write_at`].
pub type FileWriteReply = CallReply<usize>;

/// Reply delivered by [`file_fsync`].
pub type FileFsyncReply = CallReply<()>;

/// Reply delivered by [`file_size`].
pub type FileSizeReply = CallReply<u64>;

/// Reply delivered by [`file_close`].
pub type FileCloseReply = CallReply<()>;

/// Reply delivered by [`mkdir`].
pub type MkdirReply = CallReply<()>;

/// Reply delivered by [`path_metadata`].
pub type PathMetadataReply = CallReply<PathMetadata>;

/// Reply delivered by [`rename_replace`].
pub type RenameReplaceReply = CallReply<()>;

/// Reply delivered by [`remove_file`].
pub type RemoveFileReply = CallReply<()>;

/// Reply delivered by [`read_dir`].
pub type ReadDirReply = CallReply<Vec<PathBuf>>;

/// Reply delivered by [`sync_parent`].
pub type SyncParentReply = CallReply<()>;

/// Reply delivered by [`unix_bind`].
pub type UnixBindReply = CallReply<(UnixListenerId, std::path::PathBuf)>;

/// Reply delivered by [`unix_accept`].
pub type UnixAcceptReply = CallReply<UnixStreamId>;

/// Reply delivered by [`unix_connect`].
pub type UnixConnectReply = CallReply<UnixStreamId>;

/// Reply delivered by [`unix_read`].
pub type UnixReadReply = CallReply<Vec<u8>>;

/// Reply delivered by [`unix_write`].
pub type UnixWriteReply = CallReply<usize>;

/// Reply delivered by [`unix_close_listener`].
pub type UnixListenerCloseReply = CallReply<()>;

/// Reply delivered by [`unix_close_stream`].
pub type UnixStreamCloseReply = CallReply<()>;

#[cfg(test)]
mod runtime_call_tests {
    use super::*;

    #[test]
    fn into_completion_parts_preserves_terminal_completion_actions() {
        let call: RuntimeCall<()> = RuntimeCall::new_with_completion(
            CallInput::Sleep {
                after: Duration::from_millis(1),
            },
            |output| match output {
                CallOutput::TimerFired => RuntimeCallCompletion::Noop,
                other => panic!("unexpected output: {other:?}"),
            },
        );

        let (request, translator) = call.into_completion_parts();
        assert!(matches!(request, CallInput::Sleep { .. }));
        assert!(matches!(
            translator(CallOutput::TimerFired),
            RuntimeCallCompletion::Noop,
        ));
    }
}

#[cfg(test)]
mod pending_cancelable_call_set_tests {
    use std::any::TypeId;
    use std::sync::Arc;

    use super::*;

    fn token<K>(key: K, slot_id: u64) -> PendingCancelableCall<K, &'static str, ()> {
        let deferred_shared = Arc::new(tina::DeferredSlotShared::new(
            slot_id,
            TypeId::of::<&'static str>(),
        ));
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(deferred_shared),
        );
        let request = tina::runtime_internal::request_context_from_deferred(deferred);
        let call_shared = Arc::new(tina::CallHandleShared::new(TypeId::of::<()>()));
        let handle = tina::runtime_internal::call_handle_from_shared(call_shared);

        PendingCancelableCall {
            key,
            ticket: PendingCancelableTicket(slot_id),
            request,
            handle,
        }
    }

    #[test]
    fn pending_call_set_cancelable_insert_full_duplicate() {
        let mut set = PendingCancelableCallSet::with_capacity(2);
        let first = set.try_insert(token(1, 1)).expect("first insert");
        let second = set.try_insert(token(2, 2)).expect("second insert");

        assert_ne!(first, second);
        assert!(set.is_full());
        assert_eq!(set.len(), 2);

        match set.try_insert(token(1, 3)) {
            Err(PendingCancelableInsertError::DuplicateKey { token }) => {
                assert_eq!(*token.key(), 1);
                assert_eq!(token.into_request_context().slot_id(), 3);
            }
            _ => panic!("expected DuplicateKey"),
        }

        match set.try_insert(token(3, 4)) {
            Err(PendingCancelableInsertError::Full { token }) => {
                assert_eq!(*token.key(), 3);
                assert_eq!(token.into_request_context().slot_id(), 4);
            }
            _ => panic!("expected Full"),
        }

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn pending_call_set_cancelable_remove_requires_exact_ticket() {
        let mut set = PendingCancelableCallSet::with_capacity(2);
        let old_ticket = set.try_insert(token(7, 10)).expect("insert old");

        assert_eq!(
            set.remove(&99, old_ticket).unwrap_err(),
            PendingCancelableRemoveError::MissingKey
        );

        let removed = set.remove(&7, old_ticket).expect("remove old");
        assert_eq!(removed.into_request_context().slot_id(), 10);

        let new_ticket = set.try_insert(token(7, 11)).expect("reuse key");
        assert_ne!(old_ticket, new_ticket);

        assert_eq!(
            set.remove(&7, old_ticket).unwrap_err(),
            PendingCancelableRemoveError::StaleTicket,
            "old completion must not remove newer token under reused key",
        );

        let removed = set.remove(&7, new_ticket).expect("remove new");
        assert_eq!(removed.into_request_context().slot_id(), 11);
        assert!(set.is_empty());
    }

    #[test]
    fn pending_call_set_cancelable_fill_cancel_refill_shape() {
        let mut set = PendingCancelableCallSet::with_capacity(2);
        let a = set.try_insert(token(1, 1)).expect("insert a");
        let b = set.try_insert(token(2, 2)).expect("insert b");
        assert!(set.is_full());

        let _ = set.remove(&1, a).expect("cancel a");
        let _ = set.remove(&2, b).expect("cancel b");
        assert!(set.is_empty());

        set.try_insert(token(3, 3)).expect("refill a");
        set.try_insert(token(4, 4)).expect("refill b");
        assert!(set.is_full());
    }

    #[test]
    fn pending_call_set_cancelable_drain_returns_all_tokens_for_settlement() {
        let mut set = PendingCancelableCallSet::with_capacity(3);
        set.try_insert(token(1, 1)).expect("insert 1");
        set.try_insert(token(2, 2)).expect("insert 2");

        let slots: Vec<_> = set
            .drain()
            .map(|token| token.into_request_context().slot_id())
            .collect();

        assert_eq!(slots, [1, 2]);
        assert!(set.is_empty());
        set.try_insert(token(3, 3)).expect("refill after drain");
    }

    #[test]
    fn pending_call_set_cancelable_key_need_not_clone() {
        #[derive(Debug, PartialEq)]
        struct Key(u8);

        let mut set = PendingCancelableCallSet::with_capacity(1);
        let ticket = set.try_insert(token(Key(1), 1)).expect("insert");
        let removed = set.remove(&Key(1), ticket).expect("remove");

        assert_eq!(removed.into_request_context().slot_id(), 1);
    }

    #[test]
    fn owned_read_failure_decoders_return_buffers() {
        let tcp_buffer = vec![1, 2, 3, 4];
        let tcp_error = CallOutput::TcpReadBufFailed {
            buffer: tcp_buffer.clone(),
            error: CallError::ResourceBusy,
        }
        .into_tcp_read_buf()
        .expect_err("owned TCP read failure should return error envelope");
        assert_eq!(tcp_error.error, CallError::ResourceBusy);
        assert_eq!(tcp_error.buffer, tcp_buffer);

        let tls_buffer = vec![8, 9, 10];
        let tls_error = CallOutput::TlsReadBufFailed {
            buffer: tls_buffer.clone(),
            error: CallError::Timeout,
        }
        .into_tls_read_buf()
        .expect_err("owned TLS read failure should return error envelope");
        assert_eq!(tls_error.error, CallError::Timeout);
        assert_eq!(tls_error.buffer, tls_buffer);
    }

    #[test]
    fn owned_write_failure_decoders_return_bytes() {
        let tcp_bytes = b"hello".to_vec();
        let tcp_error = CallOutput::TcpWroteOwnedFailed {
            bytes: tcp_bytes.clone(),
            error: CallError::InvalidResource,
        }
        .into_tcp_wrote_owned()
        .expect_err("owned TCP write failure should return error envelope");
        assert_eq!(tcp_error.error, CallError::InvalidResource);
        assert_eq!(tcp_error.bytes, tcp_bytes);

        let tls_bytes = b"secret plaintext".to_vec();
        let tls_error = CallOutput::TlsWroteOwnedFailed {
            bytes: tls_bytes.clone(),
            error: CallError::Io,
        }
        .into_tls_wrote_owned()
        .expect_err("owned TLS write failure should return error envelope");
        assert_eq!(tls_error.error, CallError::Io);
        assert_eq!(tls_error.bytes, tls_bytes);
    }

    #[test]
    fn terminal_owned_write_decoder_names_closed_state() {
        let closed = CallOutput::TcpWroteOwnedClose {
            bytes: b"done".to_vec(),
            count: 4,
            closed: true,
        }
        .into_tcp_wrote_owned_close()
        .expect("terminal write-close success");
        assert_eq!(closed.bytes, b"done");
        assert_eq!(closed.written, 4);
        assert!(closed.closed);

        let partial = CallOutput::TcpWroteOwnedClose {
            bytes: b"partial".to_vec(),
            count: 3,
            closed: false,
        }
        .into_tcp_wrote_owned_close()
        .expect("partial terminal write stays open");
        assert_eq!(partial.bytes, b"partial");
        assert_eq!(partial.written, 3);
        assert!(!partial.closed);

        let failed = CallOutput::TcpWroteOwnedFailed {
            bytes: b"return me".to_vec(),
            error: CallError::ResourceBusy,
        }
        .into_tcp_wrote_owned_close()
        .expect_err("terminal write-close failure returns bytes");
        assert_eq!(failed.error, CallError::ResourceBusy);
        assert_eq!(failed.bytes, b"return me");
    }

    #[test]
    fn raw_map_result_treats_owned_failures_as_errors() {
        let call = RuntimeCall::map_result(
            CallInput::TcpReadBuf {
                stream: StreamId::new(1),
                buffer: Vec::new(),
                max_len: 1,
            },
            |result| result,
        );
        let (_request, translator) = call.into_parts();

        let result = translator(CallOutput::TcpReadBufFailed {
            buffer: vec![0],
            error: CallError::ResourceBusy,
        });
        assert!(matches!(result, Err(CallError::ResourceBusy)));
    }

    #[test]
    #[should_panic(expected = "capacity > 0")]
    fn pending_call_set_cancelable_zero_capacity_panics() {
        let _set: PendingCancelableCallSet<u64, (), ()> =
            PendingCancelableCallSet::with_capacity(0);
    }
}
