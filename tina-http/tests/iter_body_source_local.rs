use std::time::Duration;

use tina::prelude::*;
use tina_http::{IterBodySource, ResponseChunkMsg, ResponseChunkReply};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(1)
    }
}

#[test]
fn local_registration_keeps_chunk_calls_on_the_facade() {
    let app = LocalSystem::single_shard(TestShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("local system starts");
    let source = IterBodySource::register_local(
        &app,
        vec![b"first".to_vec(), b"second".to_vec()].into_iter(),
        4,
    )
    .expect("iterator source registers through LocalSystem");

    for expected in [Some(b"first".as_slice()), Some(b"second".as_slice()), None] {
        let outcome = app
            .call_blocking(source, ResponseChunkMsg::Next, Duration::from_secs(1))
            .expect("host call admitted");
        match (outcome, expected) {
            (CallOutcome::Replied(ResponseChunkReply::Chunk(actual)), Some(expected)) => {
                assert_eq!(actual, expected);
            }
            (CallOutcome::Replied(ResponseChunkReply::Eof), None) => {}
            (other, expected) => panic!("unexpected chunk outcome {other:?} for {expected:?}"),
        }
    }

    app.shutdown().drain().join().expect("clean shutdown");
}
