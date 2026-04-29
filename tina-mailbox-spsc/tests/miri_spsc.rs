use std::cell::Cell;
use std::rc::Rc;

use tina::{Mailbox, TrySendError};
use tina_mailbox_spsc::SpscMailbox;

#[derive(Debug)]
struct DropToken {
    drops: Rc<Cell<usize>>,
}

impl DropToken {
    fn new(drops: Rc<Cell<usize>>) -> Self {
        Self { drops }
    }
}

impl Drop for DropToken {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn fifo_wraparound_reuses_slots_without_stale_reads() {
    let mailbox = SpscMailbox::new(2);

    assert_eq!(mailbox.try_send(1), Ok(()));
    assert_eq!(mailbox.try_send(2), Ok(()));
    assert_eq!(mailbox.recv(), Some(1));
    assert_eq!(mailbox.try_send(3), Ok(()));
    assert_eq!(mailbox.recv(), Some(2));
    assert_eq!(mailbox.try_send(4), Ok(()));
    assert_eq!(mailbox.recv(), Some(3));
    assert_eq!(mailbox.recv(), Some(4));
    assert_eq!(mailbox.recv(), None);
}

#[test]
fn full_and_closed_errors_return_message_ownership() {
    let mailbox = SpscMailbox::new(1);

    assert_eq!(mailbox.try_send("first"), Ok(()));
    assert_eq!(
        mailbox.try_send("second"),
        Err(TrySendError::Full("second"))
    );
    assert_eq!(mailbox.recv(), Some("first"));

    mailbox.close();

    assert_eq!(
        mailbox.try_send("third"),
        Err(TrySendError::Closed("third"))
    );
}

#[test]
fn unread_buffered_messages_drop_exactly_once() {
    let drops = Rc::new(Cell::new(0));
    let mailbox = SpscMailbox::new(3);

    assert!(mailbox.try_send(DropToken::new(Rc::clone(&drops))).is_ok());
    assert!(mailbox.try_send(DropToken::new(Rc::clone(&drops))).is_ok());
    assert_eq!(drops.get(), 0);

    drop(mailbox);

    assert_eq!(drops.get(), 2);
}
