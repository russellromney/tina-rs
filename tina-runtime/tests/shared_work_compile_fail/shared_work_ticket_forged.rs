//! Negative fixture: a user cannot forge a `SharedWorkTicket` by
//! writing a raw struct literal. The ticket field is crate-private, so
//! user code cannot produce a ticket without first parking a caller
//! through `SharedWork::wait` or `wait_call`.

use tina_runtime::SharedWorkTicket;

fn main() {
    // No public constructor, and the only field is crate-private.
    let _forged: SharedWorkTicket<u32> = SharedWorkTicket {};
}
