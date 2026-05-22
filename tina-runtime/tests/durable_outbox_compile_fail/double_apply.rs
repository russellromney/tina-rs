//! Negative fixture: `apply` consumes the `RecordedWork` authorization, so the
//! same recorded work cannot be applied twice. Double-apply does not compile —
//! a committed-or-in-flight mutation cannot be replayed by reusing its token.

use tina_runtime::{DurableOutbox, RecordedWork};

fn main() {
    let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
    let staged = outbox.enqueue(b"work".to_vec()).expect("fits");
    let recorded: RecordedWork<Vec<u8>> = outbox
        .record(staged, Ok(()))
        .unwrap_or_else(|_| panic!("recorded"));
    let _first = outbox.apply(recorded);
    // `recorded` was moved into the first apply; applying again cannot compile.
    let _second = outbox.apply(recorded);
}
