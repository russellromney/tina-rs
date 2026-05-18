//! Negative fixture: a user cannot forge a `SharedWorkTicket` by
//! writing a raw struct literal. The `inner: WaitTicket<K>` field is
//! `pub(crate)`, so a struct literal outside `tina-runtime` cannot name
//! the field, and `WaitTicket<K>`'s fields are private too. Together
//! this means there is no path to produce a ticket without first
//! parking a caller through `SharedWork::wait` or `wait_call`.

use tina_runtime::SharedWorkTicket;

fn main() {
    // No public constructor — and `inner` is `pub(crate)` to this
    // crate, so user code outside cannot name the field.
    let _forged: SharedWorkTicket<u32> = SharedWorkTicket { inner: forbidden() };
}

fn forbidden() -> tina_runtime::WaitTicket<u32> {
    // Cannot construct a `WaitTicket` either — its fields are private.
    tina_runtime::WaitTicket {
        slot: 0,
        generation: 0,
        _key: std::marker::PhantomData,
    }
}
