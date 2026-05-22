//! Negative fixture: apply requires a `RecordedWork` — the proof that a durable
//! record landed. A freshly staged `DurableWork` that has not been recorded
//! cannot be applied. Record-before-apply is a type rule, not a convention.

use tina_runtime::{DurableOutbox, DurableWork};

fn main() {
    let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
    let staged: DurableWork<Vec<u8>> = outbox.enqueue(b"work".to_vec()).expect("fits");
    // `apply` wants `RecordedWork`, not the un-recorded `DurableWork`.
    let _ = outbox.apply(staged);
}
