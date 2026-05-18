//! Bounded member table for WebSocket rooms.
//!
//! Captures the admit / broadcast / shutdown / record_send_outcome shape
//! shared by `examples/specimen_websocket_room` and
//! `examples/systems/system_realtime_rooms`. Holds
//! `BTreeMap<WebSocketSessionId, WebSocketSessionHandle>` under a fixed
//! capacity and emits ordinary `tina::send` effects through each session's
//! connection owner. Slow-peer eviction, idle eviction, shutdown sequencing,
//! and the room's own report shape stay in the room isolate that owns this
//! table — the table only handles the bookkeeping that two specimens already
//! had verbatim.
//!
//! Not a chat framework. Not a hidden queue. Not a room registry. No `Arc`
//! sharing. The table lives as one field on a room isolate.

use std::collections::BTreeMap;

use tina::{Effect, Isolate, Outbound};
use tina_runtime::RuntimeCall;

use crate::connection::HttpConnectionMsg;
use crate::websocket::{
    WebSocketCloseCode, WebSocketSendError, WebSocketSendOutcome, WebSocketSessionHandle,
    WebSocketSessionId, WebSocketSessionMsg,
};

/// Bounded report counters captured by the table itself. Rooms can mirror
/// these into their own report shape; the table never grows a counter without
/// one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketMemberTableReport {
    pub joined: u64,
    pub admit_rejected_full: u64,
    pub admit_rejected_duplicate: u64,
    pub left_peer: u64,
    pub left_slow: u64,
    pub left_protocol: u64,
    pub left_timeout: u64,
    pub broadcast_ok: u64,
    pub broadcast_full: u64,
    pub broadcast_closed: u64,
    pub broadcast_protocol: u64,
    pub broadcast_stale: u64,
    pub broadcast_timeout: u64,
    pub shutdown_close_requested: u64,
    pub member_high_water: u64,
}

/// Outcome of an admit attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// The handle was inserted into the table.
    Admitted,
    /// The session id was already present. The new handle was not stored.
    AlreadyMember,
    /// The table was at capacity. The new handle was not stored.
    Full,
}

/// Outcome of folding a `WebSocketSendOutcome` into the table.
///
/// The room interprets these to update its own typed counters and decide
/// follow-up policy (e.g., emit a close frame, log the peer, drop a sidecar
/// liveness entry). The table itself only owns the member set and the
/// counter fields exposed via [`WebSocketMemberTableReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcomeAction {
    /// Send succeeded for a still-present member; no membership change.
    Ok,
    /// `OutboundQueueFull` / `OutboundBytesFull` from the connection owner.
    /// The member was removed; slow-peer eviction applies.
    RemovedSlow,
    /// `Closed` / `Closing` from the connection owner. The member was
    /// removed; ordinary peer-side close.
    RemovedClosed,
    /// `Protocol` from the connection owner. The member was removed; the
    /// connection saw a protocol error on this session.
    RemovedProtocol,
    /// `Timeout` from the connection owner. The member was removed.
    RemovedTimeout,
    /// `Stale` from the connection owner *or* the outcome named a session id
    /// that the table no longer holds (e.g., concurrent peer-side close
    /// during a fanout). No membership change.
    Stale,
}

/// Bounded member table for a single WebSocket room.
#[derive(Debug)]
pub struct WebSocketMemberTable {
    members: BTreeMap<WebSocketSessionId, WebSocketSessionHandle>,
    capacity: usize,
    report: WebSocketMemberTableReport,
}

impl WebSocketMemberTable {
    /// Build a table with the supplied member capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            members: BTreeMap::new(),
            capacity,
            report: WebSocketMemberTableReport::default(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.members.len() >= self.capacity
    }

    pub fn report(&self) -> WebSocketMemberTableReport {
        self.report
    }

    pub fn contains(&self, id: WebSocketSessionId) -> bool {
        self.members.contains_key(&id)
    }

    pub fn get(&self, id: WebSocketSessionId) -> Option<WebSocketSessionHandle> {
        self.members.get(&id).copied()
    }

    pub fn members(
        &self,
    ) -> impl Iterator<Item = (WebSocketSessionId, WebSocketSessionHandle)> + '_ {
        self.members.iter().map(|(id, h)| (*id, *h))
    }

    /// Attempt to admit a session.
    pub fn admit(&mut self, handle: WebSocketSessionHandle) -> AdmitOutcome {
        let id = handle.session_id();
        if self.members.contains_key(&id) {
            self.report.admit_rejected_duplicate += 1;
            return AdmitOutcome::AlreadyMember;
        }
        if self.members.len() >= self.capacity {
            self.report.admit_rejected_full += 1;
            return AdmitOutcome::Full;
        }
        self.members.insert(id, handle);
        self.report.joined += 1;
        let len = self.members.len() as u64;
        if len > self.report.member_high_water {
            self.report.member_high_water = len;
        }
        AdmitOutcome::Admitted
    }

    /// Remove a session because the peer closed / the connection owner
    /// reported close. Returns the handle if present.
    ///
    /// Counted under `left_peer`. For slow-peer eviction the table updates
    /// `left_slow` via [`Self::record_send_outcome`] instead. For
    /// admin/manual removal use [`Self::remove`].
    pub fn remove_peer(&mut self, id: WebSocketSessionId) -> Option<WebSocketSessionHandle> {
        let handle = self.members.remove(&id)?;
        self.report.left_peer += 1;
        Some(handle)
    }

    /// Remove a session without touching any counters. The room owns the
    /// reason in this path (e.g., idle-tick eviction with its own
    /// `left_idle` counter).
    pub fn remove(&mut self, id: WebSocketSessionId) -> Option<WebSocketSessionHandle> {
        self.members.remove(&id)
    }

    /// Build `tina::send` effects for `message_for` applied to every member
    /// except `exclude`. The followup `WebSocketSessionMsg::SendOutcome`
    /// routes to the room's `handle` entry point through each connection
    /// owner; pair with [`Self::record_send_outcome`] for slow-peer
    /// eviction.
    pub fn fanout<I, F>(
        &self,
        exclude: Option<WebSocketSessionId>,
        message_for: F,
    ) -> Vec<Effect<I>>
    where
        I: Isolate<
                Message = WebSocketSessionMsg,
                Send = Outbound<HttpConnectionMsg>,
                Call = RuntimeCall<WebSocketSessionMsg>,
            >,
        F: Fn(WebSocketSessionHandle) -> HttpConnectionMsg,
    {
        let mut effects = Vec::with_capacity(self.members.len());
        for (id, handle) in &self.members {
            if exclude == Some(*id) {
                continue;
            }
            effects.push(tina::send(handle.target(), message_for(*handle)));
        }
        effects
    }

    /// Broadcast a text frame to every member except `exclude`.
    pub fn broadcast_text<I>(
        &self,
        exclude: Option<WebSocketSessionId>,
        text: impl Into<String>,
    ) -> Vec<Effect<I>>
    where
        I: Isolate<
                Message = WebSocketSessionMsg,
                Send = Outbound<HttpConnectionMsg>,
                Call = RuntimeCall<WebSocketSessionMsg>,
            >,
    {
        let text = text.into();
        self.fanout(exclude, move |handle| handle.text(text.clone()))
    }

    /// Broadcast a binary frame to every member except `exclude`.
    pub fn broadcast_binary<I>(
        &self,
        exclude: Option<WebSocketSessionId>,
        bytes: impl Into<Vec<u8>>,
    ) -> Vec<Effect<I>>
    where
        I: Isolate<
                Message = WebSocketSessionMsg,
                Send = Outbound<HttpConnectionMsg>,
                Call = RuntimeCall<WebSocketSessionMsg>,
            >,
    {
        let bytes = bytes.into();
        self.fanout(exclude, move |handle| handle.binary(bytes.clone()))
    }

    /// Emit close frames toward every member. The table is left populated so
    /// subsequent `SessionClosed` / `SendOutcome` deliveries reconcile
    /// through the ordinary paths. Each call counts every targeted member
    /// under `shutdown_close_requested`.
    pub fn shutdown_close<I>(
        &mut self,
        code: Option<WebSocketCloseCode>,
        reason: impl Into<Vec<u8>>,
    ) -> Vec<Effect<I>>
    where
        I: Isolate<
                Message = WebSocketSessionMsg,
                Send = Outbound<HttpConnectionMsg>,
                Call = RuntimeCall<WebSocketSessionMsg>,
            >,
    {
        let reason = reason.into();
        let effects = self.fanout(None, move |handle| handle.close(code, reason.clone()));
        self.report.shutdown_close_requested += effects.len() as u64;
        effects
    }

    /// Fold a `WebSocketSessionMsg::SendOutcome` into the table.
    pub fn record_send_outcome(&mut self, outcome: &WebSocketSendOutcome) -> SendOutcomeAction {
        let id = outcome.session;
        if !self.members.contains_key(&id) {
            self.report.broadcast_stale += 1;
            return SendOutcomeAction::Stale;
        }
        match outcome.result {
            Ok(()) => {
                self.report.broadcast_ok += 1;
                SendOutcomeAction::Ok
            }
            Err(WebSocketSendError::OutboundQueueFull)
            | Err(WebSocketSendError::OutboundBytesFull) => {
                self.report.broadcast_full += 1;
                self.report.left_slow += 1;
                self.members.remove(&id);
                SendOutcomeAction::RemovedSlow
            }
            Err(WebSocketSendError::Closed) | Err(WebSocketSendError::Closing) => {
                self.report.broadcast_closed += 1;
                self.report.left_peer += 1;
                self.members.remove(&id);
                SendOutcomeAction::RemovedClosed
            }
            Err(WebSocketSendError::Protocol) => {
                self.report.broadcast_protocol += 1;
                self.report.left_protocol += 1;
                self.members.remove(&id);
                SendOutcomeAction::RemovedProtocol
            }
            Err(WebSocketSendError::Timeout) => {
                self.report.broadcast_timeout += 1;
                self.report.left_timeout += 1;
                self.members.remove(&id);
                SendOutcomeAction::RemovedTimeout
            }
            Err(WebSocketSendError::Stale) => {
                self.report.broadcast_stale += 1;
                SendOutcomeAction::Stale
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::{Address, AddressGeneration, IsolateId, ShardId};

    fn dummy_handle(isolate: u64) -> WebSocketSessionHandle {
        let target: Address<HttpConnectionMsg, crate::streaming::RequestChunkReply> =
            Address::new_with_generation(
                ShardId::new(0),
                IsolateId::new(isolate),
                AddressGeneration::new(0),
            );
        let session = WebSocketSessionId::new(0);
        WebSocketSessionHandle::new(session, target)
    }

    #[test]
    fn admit_fills_then_rejects_full() {
        let mut table = WebSocketMemberTable::new(2);
        assert_eq!(table.admit(dummy_handle(1)), AdmitOutcome::Admitted);
        assert_eq!(table.admit(dummy_handle(2)), AdmitOutcome::Admitted);
        assert_eq!(table.admit(dummy_handle(3)), AdmitOutcome::Full);
        assert_eq!(table.len(), 2);
        assert_eq!(table.report().joined, 2);
        assert_eq!(table.report().admit_rejected_full, 1);
        assert_eq!(table.report().member_high_water, 2);
    }

    #[test]
    fn admit_duplicate_is_typed_not_silent() {
        let mut table = WebSocketMemberTable::new(4);
        let h = dummy_handle(7);
        assert_eq!(table.admit(h), AdmitOutcome::Admitted);
        assert_eq!(table.admit(h), AdmitOutcome::AlreadyMember);
        assert_eq!(table.report().joined, 1);
        assert_eq!(table.report().admit_rejected_duplicate, 1);
    }

    #[test]
    fn record_send_outcome_slow_peer_evicts_and_counts() {
        let mut table = WebSocketMemberTable::new(2);
        let h = dummy_handle(1);
        let id = h.session_id();
        table.admit(h);
        let outcome = WebSocketSendOutcome {
            session: id,
            result: Err(WebSocketSendError::OutboundQueueFull),
        };
        assert_eq!(
            table.record_send_outcome(&outcome),
            SendOutcomeAction::RemovedSlow
        );
        assert_eq!(table.len(), 0);
        assert_eq!(table.report().left_slow, 1);
        assert_eq!(table.report().broadcast_full, 1);
    }

    #[test]
    fn record_send_outcome_stale_does_not_remove() {
        let mut table = WebSocketMemberTable::new(2);
        let h = dummy_handle(1);
        table.admit(h);
        let outcome = WebSocketSendOutcome {
            session: dummy_handle(99).session_id(),
            result: Err(WebSocketSendError::Stale),
        };
        assert_eq!(
            table.record_send_outcome(&outcome),
            SendOutcomeAction::Stale
        );
        assert_eq!(table.len(), 1);
        assert_eq!(table.report().broadcast_stale, 1);
    }

    #[test]
    fn record_send_outcome_protocol_evicts() {
        let mut table = WebSocketMemberTable::new(2);
        let h = dummy_handle(1);
        let id = h.session_id();
        table.admit(h);
        let outcome = WebSocketSendOutcome {
            session: id,
            result: Err(WebSocketSendError::Protocol),
        };
        assert_eq!(
            table.record_send_outcome(&outcome),
            SendOutcomeAction::RemovedProtocol
        );
        assert_eq!(table.len(), 0);
        assert_eq!(table.report().left_protocol, 1);
    }

    #[test]
    fn record_send_outcome_ok_keeps_member_and_counts() {
        let mut table = WebSocketMemberTable::new(2);
        let h = dummy_handle(1);
        let id = h.session_id();
        table.admit(h);
        let outcome = WebSocketSendOutcome {
            session: id,
            result: Ok(()),
        };
        assert_eq!(table.record_send_outcome(&outcome), SendOutcomeAction::Ok);
        assert_eq!(table.len(), 1);
        assert_eq!(table.report().broadcast_ok, 1);
    }

    #[test]
    fn remove_peer_decrements_and_counts() {
        let mut table = WebSocketMemberTable::new(2);
        let h = dummy_handle(1);
        let id = h.session_id();
        table.admit(h);
        assert!(table.remove_peer(id).is_some());
        assert!(table.is_empty());
        assert_eq!(table.report().left_peer, 1);
        assert!(table.remove_peer(id).is_none());
    }

    #[test]
    fn shutdown_close_counts_targets() {
        let mut table = WebSocketMemberTable::new(4);
        table.admit(dummy_handle(1));
        table.admit(dummy_handle(2));
        // The room isolate type would normally be passed; use the existing
        // test isolate from the websocket_helper module via a trivial impl
        // would require more infra here. Instead just verify counting
        // happens by checking the counter delta — fanout returns one effect
        // per member; we ignore the effects themselves.
        let count_before = table.report().shutdown_close_requested;
        let _effects: Vec<Effect<DummyRoomIsolate>> =
            table.shutdown_close(Some(WebSocketCloseCode(1001)), b"bye".to_vec());
        let count_after = table.report().shutdown_close_requested;
        assert_eq!(count_after - count_before, 2);
    }

    struct DummyRoomIsolate;

    impl Isolate for DummyRoomIsolate {
        tina::isolate_types! {
            message: WebSocketSessionMsg,
            reply: crate::websocket::WebSocketSessionOutcome,
            send: tina::Outbound<HttpConnectionMsg>,
            spawn: std::convert::Infallible,
            call: RuntimeCall<WebSocketSessionMsg>,
            shard: DummyShard,
        }

        fn handle(
            &mut self,
            _msg: WebSocketSessionMsg,
            _ctx: &mut tina::Context<'_, DummyShard, Self::Reply>,
        ) -> Effect<Self> {
            tina::reply(crate::websocket::WebSocketSessionOutcome::None)
        }
    }

    #[derive(Debug, Default, Clone, Copy)]
    struct DummyShard;

    impl tina::Shard for DummyShard {
        fn id(&self) -> ShardId {
            ShardId::new(0xd1d1)
        }
    }
}
