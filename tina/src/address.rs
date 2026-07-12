//! Address vocabulary for the tina core.
//!
//! This module owns the address/id/generation types, the outbound wrapper,
//! and the service-shaped event/request addresses. Everything here is
//! re-exported from the crate root for backwards compatibility (see
//! `lib.rs`).
//!
//! ## Module map
//!
//! - [`Address`] / [`SendAddress`] / [`CallAddress`] — typed addresses for
//!   sends and calls.
//! - [`ServiceMessage`] / [`ServiceOutbound`] / [`ServiceEventAddress`] /
//!   [`ServiceRequestAddress`] — service-shaped event vs request routing.
//! - [`Outbound`] — the (destination, message) pair returned by send
//!   constructors.
//! - [`ShardId`] / [`IsolateId`] / [`AddressGeneration`] — opaque newtype
//!   identifiers used by every address.

use std::marker::PhantomData;

type AddressMarker<M, R> = PhantomData<fn(M, R) -> (M, R)>;

/// Typed address for one isolate mailbox incarnation.
///
/// The message type parameter makes invalid sends unrepresentable at the call
/// site: an `Address<HttpMsg>` cannot be used where `Address<AuditEvent>` is
/// required.
///
/// The reply type parameter is used by runtime crates that support
/// isolate-to-isolate calls. Ordinary sends ignore it, but `call(target, ...)`
/// can infer the target's reply type from `Address<Message, Reply>` instead of
/// trusting a separate turbofish at the call site.
///
/// An address identifies one incarnation of an isolate: shard id, isolate id,
/// and generation. Runtime-issued addresses should be preferred for real
/// delivery. Manually constructed addresses are useful in tests and examples,
/// but may be stale or unknown to a runtime.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address<M, R = ()> {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
    marker: AddressMarker<M, R>,
}

impl<M, R> Copy for Address<M, R> {}

impl<M, R> Clone for Address<M, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> Address<M> {
    /// Creates a new typed address for the initial generation.
    pub const fn new(shard: ShardId, isolate: IsolateId) -> Self {
        Self::new_with_generation(shard, isolate, AddressGeneration::new(0))
    }
}

impl<M, R> Address<M, R> {
    /// Creates a new typed address from shard, isolate, and generation.
    pub const fn new_with_generation(
        shard: ShardId,
        isolate: IsolateId,
        generation: AddressGeneration,
    ) -> Self {
        Self {
            shard,
            isolate,
            generation,
            marker: PhantomData,
        }
    }

    /// Returns the shard that owns this address.
    pub const fn shard(self) -> ShardId {
        self.shard
    }

    /// Returns the isolate identifier on the owning shard.
    pub const fn isolate(self) -> IsolateId {
        self.isolate
    }

    /// Returns the isolate generation this address targets.
    pub const fn generation(self) -> AddressGeneration {
        self.generation
    }

    /// Returns the same runtime address with a different reply marker.
    ///
    /// Runtime-issued addresses already carry the right reply type. This is
    /// mostly useful for tests that manually construct addresses.
    pub const fn with_reply<Reply>(self) -> Address<M, Reply> {
        Address::new_with_generation(self.shard, self.isolate, self.generation)
    }
}

/// A send-only capability for one isolate.
///
/// `SendAddress<M>` is the compile-time rail for "send a message and do not
/// expect a reply." It wraps an [`Address<M, ()>`](Address) and refuses to be
/// converted into a [`CallAddress`]: a function that takes a `CallAddress`
/// cannot accept a `SendAddress`, so the wrong path is a compile error rather
/// than a runtime rejection.
///
/// Construct one through [`Address::send_only`] or through the typed handles
/// returned by `tina_runtime::Runtime::register_service`. The escape hatch
/// [`SendAddress::address`] returns the underlying raw [`Address`] for the rare
/// case where low-level access is required.
///
/// ```compile_fail
/// # use tina::{Address, IsolateId, SendAddress, ShardId};
/// # enum Msg { Tick }
/// # struct Reply;
/// fn want_call(_call: tina::CallAddress<Msg, Reply>) {}
///
/// let raw: Address<Msg, Reply> =
///     Address::new_with_generation(ShardId::new(0), IsolateId::new(1), tina::AddressGeneration::new(0));
/// let send_only: SendAddress<Msg> = raw.send_only();
/// // SendAddress is not a CallAddress, so this does not compile.
/// want_call(send_only);
/// ```
#[derive(Debug)]
#[repr(transparent)]
pub struct SendAddress<M> {
    address: Address<M, ()>,
}

impl<M> Copy for SendAddress<M> {}

impl<M> Clone for SendAddress<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> PartialEq for SendAddress<M> {
    fn eq(&self, other: &Self) -> bool {
        self.address.shard() == other.address.shard()
            && self.address.isolate() == other.address.isolate()
            && self.address.generation() == other.address.generation()
    }
}

impl<M> Eq for SendAddress<M> {}

impl<M> SendAddress<M> {
    /// Wraps a runtime-issued [`Address`] as a send-only capability.
    ///
    /// The reply marker is erased: a `SendAddress` cannot recover a typed
    /// reply lane and is therefore not callable through `tina_runtime::call`.
    pub const fn from_address<R>(address: Address<M, R>) -> Self {
        Self {
            address: address.with_reply::<()>(),
        }
    }

    /// Returns the underlying raw [`Address`].
    ///
    /// This is the escape hatch for low-level code that needs the original
    /// [`Address`]. Prefer keeping `SendAddress` at the boundary so wrong-path
    /// calls remain compile-time errors.
    pub const fn address(self) -> Address<M, ()> {
        self.address
    }

    /// Returns the shard that owns this address.
    pub const fn shard(self) -> ShardId {
        self.address.shard()
    }

    /// Returns the isolate identifier on the owning shard.
    pub const fn isolate(self) -> IsolateId {
        self.address.isolate()
    }

    /// Returns the isolate generation this address targets.
    pub const fn generation(self) -> AddressGeneration {
        self.address.generation()
    }
}

/// A callable capability for one isolate.
///
/// `CallAddress<M, R>` is the compile-time rail for "send a message and wait
/// for a reply of type `R`." Runtime helpers like
/// `tina_runtime::call_typed` accept only `CallAddress`, so passing a
/// [`SendAddress`] (or a raw [`Address`] without the explicit upgrade) is a
/// compile error.
///
/// Construct one through [`Address::callable`] or through the typed handles
/// returned by `tina_runtime::Runtime::register_service`. The escape hatch
/// [`CallAddress::address`] returns the underlying raw [`Address`].
///
/// ```compile_fail
/// # use std::time::Duration;
/// # use tina::{Address, IsolateId, SendAddress, ShardId};
/// # enum Msg { Tick }
/// # struct Reply;
/// fn want_send(_send: SendAddress<Msg>) {}
///
/// let raw: Address<Msg, Reply> =
///     Address::new_with_generation(ShardId::new(0), IsolateId::new(1), tina::AddressGeneration::new(0));
/// let callable: tina::CallAddress<Msg, Reply> = raw.callable();
/// // CallAddress is not a SendAddress, so this does not compile.
/// want_send(callable);
/// ```
#[derive(Debug)]
#[repr(transparent)]
pub struct CallAddress<M, R> {
    address: Address<M, R>,
}

impl<M, R> Copy for CallAddress<M, R> {}

impl<M, R> Clone for CallAddress<M, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M, R> PartialEq for CallAddress<M, R> {
    fn eq(&self, other: &Self) -> bool {
        self.address.shard() == other.address.shard()
            && self.address.isolate() == other.address.isolate()
            && self.address.generation() == other.address.generation()
    }
}

impl<M, R> Eq for CallAddress<M, R> {}

impl<M, R> CallAddress<M, R> {
    /// Wraps a runtime-issued [`Address`] as a callable capability.
    pub const fn from_address(address: Address<M, R>) -> Self {
        Self { address }
    }

    /// Returns the underlying raw [`Address`].
    ///
    /// This is the escape hatch for low-level code that needs the original
    /// [`Address<M, R>`]. Prefer keeping `CallAddress` at the boundary so the
    /// wrong-path "send a callable as if it were send-only" is a compile error.
    pub const fn address(self) -> Address<M, R> {
        self.address
    }

    /// Returns the shard that owns this address.
    pub const fn shard(self) -> ShardId {
        self.address.shard()
    }

    /// Returns the isolate identifier on the owning shard.
    pub const fn isolate(self) -> IsolateId {
        self.address.isolate()
    }

    /// Returns the isolate generation this address targets.
    pub const fn generation(self) -> AddressGeneration {
        self.address.generation()
    }
}

impl<M, R> Address<M, R> {
    /// Returns a send-only capability for this address.
    ///
    /// The reply marker is erased so the resulting [`SendAddress`] cannot be
    /// used as a [`CallAddress`].
    pub const fn send_only(self) -> SendAddress<M> {
        SendAddress::from_address(self)
    }

    /// Returns a callable capability for this address.
    pub const fn callable(self) -> CallAddress<M, R> {
        CallAddress::from_address(self)
    }
}

/// Message envelope used by the split-service authoring path.
///
/// `Event` values are mailbox facts. `Request` values carry caller authority.
/// User code normally does not construct this enum directly; it uses
/// [`crate::send_event`] and `tina_runtime::call_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceMessage<Event, Request> {
    /// Fire-and-forget mailbox event.
    Event(Event),
    /// Callable request with caller authority.
    Request(Request),
}

/// Outbound effect capability for a split event/request service.
///
/// Use this as an isolate's `send` type when it calls [`crate::send_event`]
/// and `tina_runtime::call_request` against split-service handles. The alias
/// keeps the routing envelope out of application-facing associated types while
/// preserving the exact event and request capabilities.
///
/// ```
/// # use tina::prelude::*;
/// # use tina::ServiceEventAddress;
/// # enum Event { Refresh }
/// # enum Request { Read }
/// # enum ClientMessage { Refresh }
/// # struct Client {
/// #     events: ServiceEventAddress<Event, Request>,
/// # }
/// #[tina::isolate(
///     message = ClientMessage,
///     send = ServiceOutbound<Event, Request>,
/// )]
/// impl Client {
///     fn handle(
///         &mut self,
///         message: ClientMessage,
///         _ctx: &mut Context<'_, SingleShard, Self::Reply>,
///     ) -> Effect<Self> {
///         match message {
///             ClientMessage::Refresh => tina::send_event(self.events, Event::Refresh),
///         }
///     }
/// }
/// ```
pub type ServiceOutbound<Event, Request> = Outbound<ServiceMessage<Event, Request>>;

/// Send capability for split-service events.
#[derive(Debug)]
#[repr(transparent)]
pub struct ServiceEventAddress<Event, Request> {
    address: SendAddress<ServiceMessage<Event, Request>>,
}

impl<Event, Request> Copy for ServiceEventAddress<Event, Request> {}

impl<Event, Request> Clone for ServiceEventAddress<Event, Request> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Event, Request> ServiceEventAddress<Event, Request> {
    /// Wraps a send address for the split service envelope.
    pub const fn from_send_address(address: SendAddress<ServiceMessage<Event, Request>>) -> Self {
        Self { address }
    }

    /// Returns the underlying send capability.
    pub const fn address(self) -> SendAddress<ServiceMessage<Event, Request>> {
        self.address
    }
}

/// Call capability for split-service requests.
#[derive(Debug)]
#[repr(transparent)]
pub struct ServiceRequestAddress<Event, Request, Reply> {
    address: CallAddress<ServiceMessage<Event, Request>, Reply>,
}

impl<Event, Request, Reply> Copy for ServiceRequestAddress<Event, Request, Reply> {}

impl<Event, Request, Reply> Clone for ServiceRequestAddress<Event, Request, Reply> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Event, Request, Reply> ServiceRequestAddress<Event, Request, Reply> {
    /// Wraps a call address for the split service envelope.
    pub const fn from_call_address(
        address: CallAddress<ServiceMessage<Event, Request>, Reply>,
    ) -> Self {
        Self { address }
    }

    /// Returns the underlying call capability.
    pub const fn address(self) -> CallAddress<ServiceMessage<Event, Request>, Reply> {
        self.address
    }
}

/// ```compile_fail
/// use tina::{Address, IsolateId, Outbound, ShardId};
///
/// enum HttpEvent {
///     Request,
/// }
///
/// enum AuditEvent {
///     Event,
/// }
///
/// let http_only = Address::<HttpEvent>::new(ShardId::new(0), IsolateId::new(7));
/// let _invalid = Outbound::new(http_only, AuditEvent::Event);
/// ```
/// A typed outbound send request.
///
/// `Outbound` is intentionally not `Clone`/`PartialEq`. Real message
/// types are often non-`Clone` (`Bytes`, file handles, large buffers), and
/// a send request is meant to be moved into the runtime, not duplicated.
#[must_use = "a send request has no effect until a runtime executes it"]
#[derive(Debug)]
pub struct Outbound<M> {
    destination: Address<M>,
    message: M,
}

impl<M> Outbound<M> {
    /// Creates a new outbound send request.
    pub fn new<R>(destination: Address<M, R>, message: M) -> Self {
        Self {
            destination: destination.with_reply::<()>(),
            message,
        }
    }

    /// Returns the destination address.
    pub const fn destination(&self) -> Address<M> {
        self.destination
    }

    /// Returns a shared reference to the outbound message.
    pub const fn message(&self) -> &M {
        &self.message
    }

    /// Splits the request into its destination and message payload.
    pub fn into_parts(self) -> (Address<M>, M) {
        (self.destination, self.message)
    }
}

/// Logical identifier for a shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(u32);

impl ShardId {
    /// Creates a shard identifier from a raw integer.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw shard identifier.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Logical identifier for an isolate within the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IsolateId(u64);

impl IsolateId {
    /// Creates an isolate identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw isolate identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Generation for one isolate identifier.
///
/// The initial generation is `AddressGeneration::new(0)`. Runtime crates can
/// use later generations to reject stale addresses if an isolate identifier is
/// ever reused for a replacement incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AddressGeneration(u64);

impl AddressGeneration {
    /// Creates an address generation from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw generation identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}
