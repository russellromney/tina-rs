use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::{Mailbox, TrySendError};
use tina_mailbox_spsc::SpscMailbox;

#[derive(Debug)]
struct DropSpy {
    drops: Arc<AtomicUsize>,
}

impl DropSpy {
    fn new(drops: Arc<AtomicUsize>) -> Self {
        Self { drops }
    }
}

impl Drop for DropSpy {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn drop_counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

#[test]
fn drained_messages_drop_exactly_once_when_the_consumer_releases_them() {
    let mailbox = SpscMailbox::new(1);
    let drops = drop_counter();

    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(mailbox.try_send(DropSpy::new(Arc::clone(&drops))).is_ok());

    let message = mailbox.recv().expect("message should be present");
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    drop(mailbox);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    drop(message);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn unread_buffered_messages_drop_exactly_once_when_the_mailbox_drops() {
    let drops = drop_counter();

    {
        let mailbox = SpscMailbox::new(1);
        assert!(mailbox.try_send(DropSpy::new(Arc::clone(&drops))).is_ok());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn full_returns_ownership_without_double_drop() {
    let mailbox = SpscMailbox::new(1);
    let buffered_drops = drop_counter();
    let rejected_drops = drop_counter();

    assert!(
        mailbox
            .try_send(DropSpy::new(Arc::clone(&buffered_drops)))
            .is_ok()
    );

    let rejected = match mailbox.try_send(DropSpy::new(Arc::clone(&rejected_drops))) {
        Err(TrySendError::Full(message)) => message,
        other => panic!("expected Full error, got {other:?}"),
    };

    assert_eq!(buffered_drops.load(Ordering::SeqCst), 0);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);

    drop(rejected);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);

    drop(mailbox);
    assert_eq!(buffered_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn closed_returns_ownership_without_double_drop() {
    let mailbox = SpscMailbox::new(1);
    let rejected_drops = drop_counter();

    mailbox.close();

    let rejected = match mailbox.try_send(DropSpy::new(Arc::clone(&rejected_drops))) {
        Err(TrySendError::Closed(message)) => message,
        other => panic!("expected Closed error, got {other:?}"),
    };

    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    drop(rejected);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
}
