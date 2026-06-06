use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use tina::{Mailbox, TrySendError};
use tina_mailbox_spsc::SpscMailbox;

#[test]
fn mailbox_preserves_fifo_order() {
    let mailbox = SpscMailbox::new(4);

    assert_eq!(mailbox.capacity(), 4);
    assert_eq!(mailbox.try_send("first"), Ok(()));
    assert_eq!(mailbox.try_send("second"), Ok(()));
    assert_eq!(mailbox.try_send("third"), Ok(()));

    assert_eq!(mailbox.recv(), Some("first"));
    assert_eq!(mailbox.recv(), Some("second"));
    assert_eq!(mailbox.recv(), Some("third"));
    assert_eq!(mailbox.recv(), None);
}

#[test]
#[should_panic(expected = "SpscMailbox capacity must be a power of two")]
fn mailbox_rejects_non_power_of_two_capacity() {
    let _mailbox = SpscMailbox::<usize>::new(3);
}

#[test]
fn mailbox_returns_full_without_buffering_overflowed_messages() {
    let mailbox = SpscMailbox::new(1);

    assert_eq!(mailbox.try_send("first"), Ok(()));
    assert_eq!(
        mailbox.try_send("second"),
        Err(TrySendError::Full("second"))
    );
    assert_eq!(mailbox.recv(), Some("first"));
    assert_eq!(mailbox.recv(), None);

    assert_eq!(mailbox.try_send("third"), Ok(()));
    assert_eq!(mailbox.recv(), Some("third"));
    assert_eq!(mailbox.recv(), None);
}

#[test]
fn close_rejects_future_sends_but_drains_buffered_messages() {
    let mailbox = SpscMailbox::new(2);

    assert_eq!(mailbox.try_send("buffered"), Ok(()));
    mailbox.close();

    assert_eq!(
        mailbox.try_send("rejected"),
        Err(TrySendError::Closed("rejected"))
    );
    assert_eq!(mailbox.recv(), Some("buffered"));
    assert_eq!(mailbox.recv(), None);
}

#[test]
fn wake_hook_runs_only_on_empty_to_nonempty_transition() {
    let mailbox = SpscMailbox::new(4);
    let wakes = Arc::new(AtomicUsize::new(0));
    let wakes_for_hook = Arc::clone(&wakes);
    mailbox.set_wake_hook(Some(Arc::new(move || {
        wakes_for_hook.fetch_add(1, Ordering::Relaxed);
    })));

    assert_eq!(mailbox.try_send("first"), Ok(()));
    assert_eq!(mailbox.try_send("second"), Ok(()));
    assert_eq!(wakes.load(Ordering::Relaxed), 1);

    assert_eq!(mailbox.recv(), Some("first"));
    assert_eq!(mailbox.try_send("third"), Ok(()));
    assert_eq!(wakes.load(Ordering::Relaxed), 1);

    assert_eq!(mailbox.recv(), Some("second"));
    assert_eq!(mailbox.recv(), Some("third"));
    assert_eq!(mailbox.recv(), None);
    assert_eq!(mailbox.try_send("fourth"), Ok(()));
    assert_eq!(wakes.load(Ordering::Relaxed), 2);
}

#[test]
fn mailbox_accepts_dst_backed_payloads_via_owning_pointers() {
    let mailbox: SpscMailbox<Box<str>> = SpscMailbox::new(2);

    assert_eq!(mailbox.try_send("hello tina".into()), Ok(()));
    assert_eq!(mailbox.recv().as_deref(), Some("hello tina"));
    assert_eq!(mailbox.recv(), None);
}

#[test]
fn single_producer_and_consumer_exchange_fifo_stream() {
    let mailbox = Arc::new(SpscMailbox::new(8));
    let producer_mailbox = Arc::clone(&mailbox);
    let consumer_mailbox = Arc::clone(&mailbox);

    let producer = thread::spawn(move || {
        let mut next = 0usize;

        while next < 256 {
            match producer_mailbox.try_send(next) {
                Ok(()) => next += 1,
                Err(TrySendError::Full(returned)) => {
                    assert_eq!(returned, next);
                    thread::yield_now();
                }
                Err(TrySendError::Closed(_)) => panic!("mailbox closed during producer test"),
            }
        }
    });

    let consumer = thread::spawn(move || {
        let mut received = Vec::new();

        while received.len() < 256 {
            match consumer_mailbox.recv() {
                Some(value) => received.push(value),
                None => thread::yield_now(),
            }
        }

        received
    });

    producer
        .join()
        .expect("producer thread should finish cleanly");
    let received = consumer
        .join()
        .expect("consumer thread should finish cleanly");

    assert_eq!(received, (0..256).collect::<Vec<_>>());
}
