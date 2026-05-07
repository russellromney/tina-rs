//! Deferred reply slot bookkeeping for the simulator.
//!
//! Mirrors `tina_runtime::deferred` so the simulator and live runtime
//! agree on the externally visible meaning of capture, send, drop,
//! caller-closed, and reply-path failures. Slot-id allocation and
//! pending-capture handoff live in [`tina::DeferredSlotRegistry`]; the
//! simulator owns its own promoted-slot table for routing and sweeps.

use std::sync::Arc;

use tina::{DeferredSlotShared, IsolateId};

use tina_runtime::{CallId, DeferredSlotId};

use crate::internals::RegisteredAddress;

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeferredRouting {
    Local,
    /// Reserved: first form refuses cross-shard captures, so this
    /// variant is currently unreachable from user code.
    #[allow(dead_code)]
    Remote {
        requester: RegisteredAddress,
        cause: tina_runtime::CauseId,
    },
}

pub(crate) struct DeferredSlotRecord {
    pub slot_id: DeferredSlotId,
    pub call_id: CallId,
    pub capturing_isolate: IsolateId,
    pub shared: Arc<DeferredSlotShared>,
    pub routing: DeferredRouting,
}

#[derive(Default)]
pub(crate) struct PromotedSlots {
    slots: Vec<DeferredSlotRecord>,
}

impl PromotedSlots {
    pub fn push(&mut self, record: DeferredSlotRecord) {
        self.slots.push(record);
    }

    pub fn take_by_handle(
        &mut self,
        shared: &Arc<DeferredSlotShared>,
    ) -> Option<DeferredSlotRecord> {
        let pos = self
            .slots
            .iter()
            .position(|s| Arc::ptr_eq(&s.shared, shared))?;
        Some(self.slots.remove(pos))
    }

    /// Pop the slot tracking a given local call id. See
    /// `tina_runtime::deferred::PromotedSlots::take_by_local_call_id`
    /// for the local-only invariant.
    pub fn take_by_local_call_id(&mut self, call_id: CallId) -> Option<DeferredSlotRecord> {
        let pos = self
            .slots
            .iter()
            .position(|s| matches!(s.routing, DeferredRouting::Local) && s.call_id == call_id)?;
        Some(self.slots.remove(pos))
    }

    /// Drain every slot captured by the given isolate. Called when an
    /// isolate stops so the simulator emits terminal facts eagerly.
    pub fn take_by_isolate(&mut self, isolate: IsolateId) -> Vec<DeferredSlotRecord> {
        let mut taken = Vec::new();
        let mut i = 0;
        while i < self.slots.len() {
            if self.slots[i].capturing_isolate == isolate {
                taken.push(self.slots.remove(i));
            } else {
                i += 1;
            }
        }
        taken
    }

    /// Single-pass sweep: independent Rcs cannot cascade.
    pub fn sweep_dropped(&mut self) -> Vec<DeferredSlotRecord> {
        let mut dropped = Vec::new();
        let mut i = 0;
        while i < self.slots.len() {
            if Arc::strong_count(&self.slots[i].shared) <= 1 {
                dropped.push(self.slots.remove(i));
            } else {
                i += 1;
            }
        }
        dropped
    }
}
