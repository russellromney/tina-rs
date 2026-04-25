#![cfg(feature = "loom")]

use loom::sync::Arc;
use loom::thread;
use tina::{Mailbox, TrySendError};
use tina_mailbox_spsc::SpscMailbox;

fn bounded_model<F>(f: F)
where
    F: Fn() + Sync + Send + 'static,
{
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;
    builder.preemption_bound = Some(3);
    builder.check(f);
}

#[test]
fn close_waits_for_a_racing_successful_send_to_become_visible() {
    loom::model(|| {
        let mailbox = Arc::new(SpscMailbox::new(1));
        let producer_mailbox = Arc::clone(&mailbox);
        let closer_mailbox = Arc::clone(&mailbox);

        let producer = thread::spawn(move || producer_mailbox.try_send(1usize));
        let closer = thread::spawn(move || closer_mailbox.close());

        closer.join().expect("closer thread should finish cleanly");

        let observed_after_close = mailbox.recv();
        let send_result = producer
            .join()
            .expect("producer thread should finish cleanly");
        let observed_after_producer = mailbox.recv();

        match send_result {
            Ok(()) => {
                assert_eq!(observed_after_close, Some(1));
                assert_eq!(observed_after_producer, None);
            }
            Err(TrySendError::Closed(returned)) => {
                assert_eq!(returned, 1);
                assert_eq!(observed_after_close, None);
                assert_eq!(observed_after_producer, None);
            }
            Err(TrySendError::Full(_)) => {
                panic!("capacity-1 mailbox should not report Full in this model");
            }
        }
    });
}

#[test]
fn close_racing_with_recv_still_preserves_buffered_delivery() {
    loom::model(|| {
        let mailbox = Arc::new(SpscMailbox::new(1));
        assert_eq!(mailbox.try_send(7usize), Ok(()));

        let closer_mailbox = Arc::clone(&mailbox);
        let consumer_mailbox = Arc::clone(&mailbox);
        let closer = thread::spawn(move || closer_mailbox.close());
        let consumer = thread::spawn(move || consumer_mailbox.recv());

        closer.join().expect("closer thread should finish cleanly");
        assert_eq!(
            consumer
                .join()
                .expect("consumer thread should finish cleanly"),
            Some(7)
        );
        assert_eq!(mailbox.recv(), None);
        assert_eq!(mailbox.try_send(9usize), Err(TrySendError::Closed(9)));
    });
}

#[test]
fn wraparound_preserves_fifo_across_full_and_empty_boundaries() {
    bounded_model(|| {
        let mailbox = Arc::new(SpscMailbox::new(2));

        let producer_mailbox = Arc::clone(&mailbox);
        let consumer_mailbox = Arc::clone(&mailbox);
        let producer = thread::spawn(move || {
            let mut next = 0usize;

            while next < 4 {
                match producer_mailbox.try_send(next) {
                    Ok(()) => next += 1,
                    Err(TrySendError::Full(returned)) => {
                        assert_eq!(returned, next);
                        thread::yield_now();
                    }
                    Err(TrySendError::Closed(_)) => panic!("mailbox closed during wraparound test"),
                }
            }
        });
        let consumer = thread::spawn(move || {
            let mut received = Vec::new();

            while received.len() < 4 {
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
        assert_eq!(
            consumer
                .join()
                .expect("consumer thread should finish cleanly"),
            vec![0, 1, 2, 3]
        );
        assert_eq!(mailbox.recv(), None);
    });
}

#[test]
fn slot_reuse_is_sound_across_multiple_send_receive_rounds() {
    bounded_model(|| {
        let mailbox = Arc::new(SpscMailbox::new(1));

        let producer_mailbox = Arc::clone(&mailbox);
        let consumer_mailbox = Arc::clone(&mailbox);
        let producer = thread::spawn(move || {
            let mut next = 1usize;

            while next <= 4 {
                match producer_mailbox.try_send(next) {
                    Ok(()) => next += 1,
                    Err(TrySendError::Full(returned)) => {
                        assert_eq!(returned, next);
                        thread::yield_now();
                    }
                    Err(TrySendError::Closed(_)) => {
                        panic!("mailbox closed during multi-round reuse test")
                    }
                }
            }
        });
        let consumer = thread::spawn(move || {
            let mut received = Vec::new();

            while received.len() < 4 {
                match consumer_mailbox.recv() {
                    Some(observed) => received.push(observed),
                    None => thread::yield_now(),
                }
            }

            received
        });

        producer
            .join()
            .expect("producer thread should finish cleanly");
        assert_eq!(
            consumer
                .join()
                .expect("consumer thread should finish cleanly"),
            vec![1, 2, 3, 4]
        );

        assert_eq!(mailbox.recv(), None);
    });
}
