use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use tina_runtime::{
    CancellationSupport, LocalSystem, LocalSystemConfig, ResourceExecutionShape, ResourceSupport,
    RuntimeCapabilities, ShutdownSupport, TraceRetention,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessStatus {
    Supported,
    Partial,
    NotClaimed,
    PlatformGated,
}

#[derive(Debug, Clone, Copy)]
struct MatrixRow {
    capability: &'static str,
    status: ReadinessStatus,
    evidence: &'static str,
    tina: Option<fn(RuntimeCapabilities) -> tina_runtime::ResourceCapability>,
    tokio_note: &'static str,
    glommio_note: &'static str,
}

fn rows() -> Vec<MatrixRow> {
    vec![
        supported(
            "timer",
            "runtime-owned sleep; consumer and service tests",
            |c| c.timers,
        ),
        supported(
            "tcp",
            "live TCP echo/service tests plus simulator TCP DST",
            |c| c.tcp,
        ),
        supported("udp", "runtime-owned UDP rail with simulator replay", |c| {
            c.udp
        }),
        supported("dns", "bounded DNS lane and simulator DNS replay", |c| {
            c.dns
        }),
        supported("tls", "native TLS client/server lane tests", |c| c.tls),
        supported("file/path", "runtime-owned file/path service tests", |c| {
            c.local_file
        }),
        supported("process", "bounded process lane tests", |c| c.process),
        supported(
            "signal",
            "Unix live signal capture; typed capability elsewhere",
            |c| c.signal,
        ),
        supported(
            "local persistence",
            "snapshot/journal tests, storage-lane pressure, recovery replay",
            |c| c.local_persistence,
        ),
        supported(
            "storage lane",
            "bounded storage lane capacity and full/closed tests",
            |c| c.storage_lane,
        ),
        MatrixRow {
            capability: "isolate call",
            status: ReadinessStatus::Supported,
            evidence: "mandatory-timeout isolate calls with full/closed/timeout/reply tests",
            tina: None,
            tokio_note: "Tokio can use oneshot plus timeout, but timeout is opt-in.",
            glommio_note: "Comparable via local task/channel patterns.",
        },
        MatrixRow {
            capability: "cross-shard send",
            status: ReadinessStatus::Supported,
            evidence: "bounded shard-pair queues and live multi-shard pressure tests",
            tina: None,
            tokio_note: "Tokio mpsc can be bounded; runtime queues remain outside this contract.",
            glommio_note: "Comparable to shard-local executor message passing on Linux.",
        },
        MatrixRow {
            capability: "bridge ingress",
            status: ReadinessStatus::Partial,
            evidence: "bounded Tokio/Tower/Axum edge bridge; no middleware-inside-Tina claim",
            tina: None,
            tokio_note: "Tokio owns the edge; Tina owns isolate state after admission.",
            glommio_note: "No direct Glommio bridge claim.",
        },
        MatrixRow {
            capability: "shutdown",
            status: ReadinessStatus::Supported,
            evidence: "terminal reports name clean, failed, partial trace, pending, and owned resources",
            tina: None,
            tokio_note: "Tokio shutdown is usually application policy.",
            glommio_note: "Comparable lifecycle policy must be app-specific.",
        },
        MatrixRow {
            capability: "cancellation",
            status: ReadinessStatus::Supported,
            evidence: "requester-stop, timeout, bridge pre-admission cancel, and driver tombstone tests",
            tina: None,
            tokio_note: "Tokio cancellation is usually drop/policy shaped and library specific.",
            glommio_note: "Comparable cancellation depends on each Glommio operation surface.",
        },
        MatrixRow {
            capability: "backpressure",
            status: ReadinessStatus::Supported,
            evidence: "mailbox, ingress, shard-pair, bridge, storage, DNS, TLS, process, and signal pressure tests",
            tina: None,
            tokio_note: "Tokio channels can be bounded; executor and library queues are outside one contract.",
            glommio_note: "Glommio has strong local executor pressure ideas; Tina comparison is semantic.",
        },
        MatrixRow {
            capability: "replay/DST",
            status: ReadinessStatus::Supported,
            evidence: "saved-seed service DST with replay and deletion shrinking",
            tina: None,
            tokio_note: "Tokio has timer pause, not Tina-style message/I/O/fault replay.",
            glommio_note: "No direct equivalent claimed.",
        },
        MatrixRow {
            capability: "thread/core affinity",
            status: ReadinessStatus::Partial,
            evidence: "configured_core hard-pins the shard worker on Linux (sched_setaffinity over the allowed affinity mask, reported Applied with observed core); Unsupported on macOS, Failed for an out-of-mask core; helper lanes unpinned",
            tina: None,
            tokio_note: "Tokio multi-thread work stealing is a different ownership model.",
            glommio_note: "Glommio pins thread-per-core by default; Tina's hard pin is opt-in, Linux-only, and leaves helper lanes unpinned.",
        },
        MatrixRow {
            capability: "cost reporting",
            status: ReadinessStatus::Partial,
            evidence: "make verify runs local smoke timing rows; no benchmark thresholds or speed claim",
            tina: None,
            tokio_note: "Tokio comparison needs a later benchmark policy before performance claims.",
            glommio_note: "Glommio comparison is platform-gated and not part of default verify.",
        },
        MatrixRow {
            capability: "Tina-owned io_uring backend",
            status: ReadinessStatus::NotClaimed,
            evidence: "Betelgeuse owns the native Linux backend; Tina gates only its adapter semantics",
            tina: None,
            tokio_note: "Tokio uses platform reactors; no Tina user-level claim here.",
            glommio_note: "Glommio is Linux/io_uring-shaped; Baobab rows are platform-gated.",
        },
        MatrixRow {
            capability: "glommio comparison",
            status: ReadinessStatus::PlatformGated,
            evidence: "must skip visibly off supported Linux environments",
            tina: None,
            tokio_note: "Tokio comparison rows are portable where small and fair.",
            glommio_note: "Optional/platform-gated; never a default macOS dependency.",
        },
    ]
}

fn supported(
    capability: &'static str,
    evidence: &'static str,
    tina: fn(RuntimeCapabilities) -> tina_runtime::ResourceCapability,
) -> MatrixRow {
    MatrixRow {
        capability,
        status: ReadinessStatus::Supported,
        evidence,
        tina: Some(tina),
        tokio_note: "Comparable behavior row required where small and fair.",
        glommio_note: "Optional/platform-gated comparison.",
    }
}

#[test]
fn baobab_readiness_matrix_matches_public_capabilities() {
    let config = LocalSystemConfig {
        storage_lane_capacity: 3,
        dns_lane_capacity: 4,
        tls_lane_capacity: 5,
        process_lane_capacity: 6,
        signal_capacity: 7,
        trace_retention: TraceRetention::Bounded(64),
        shutdown_lane_drain_timeout: Duration::from_millis(25),
        ..LocalSystemConfig::default()
    };
    let app = LocalSystem::single_shard(MatrixShard(96), MatrixMailboxFactory)
        .config(config)
        .build();
    let capabilities = app.capabilities();

    assert_eq!(capabilities.storage_lane.capacity(), Some(3));
    assert_eq!(capabilities.dns.capacity(), Some(4));
    assert_eq!(capabilities.tls.capacity(), Some(5));
    assert_eq!(capabilities.process.capacity(), Some(6));
    assert_eq!(capabilities.signal.capacity(), Some(7));

    let rows = rows();
    assert!(
        rows.len() >= 18,
        "readiness matrix should cover the Baobab surface"
    );
    assert!(
        rows.iter()
            .any(|row| row.status == ReadinessStatus::Partial)
    );
    assert!(
        rows.iter()
            .any(|row| row.status == ReadinessStatus::NotClaimed)
    );
    assert!(
        rows.iter()
            .any(|row| row.status == ReadinessStatus::PlatformGated)
    );

    for row in rows {
        assert!(!row.capability.is_empty());
        assert!(
            !row.evidence.is_empty(),
            "missing evidence for {}",
            row.capability
        );
        assert!(
            !row.tokio_note.is_empty(),
            "missing Tokio note for {}",
            row.capability
        );
        assert!(
            !row.glommio_note.is_empty(),
            "missing Glommio note for {}",
            row.capability
        );
        if let Some(selector) = row.tina {
            let capability = selector(capabilities);
            assert_eq!(
                capability.support(),
                ResourceSupport::Supported,
                "{} should match RuntimeCapabilities support",
                row.capability
            );
            assert_ne!(
                capability.execution(),
                ResourceExecutionShape::NotApplicable,
                "{} should name execution shape",
                row.capability
            );
            assert_ne!(
                capability.cancellation(),
                CancellationSupport::NotApplicable,
                "{} should name cancellation shape",
                row.capability
            );
            assert_ne!(
                capability.shutdown(),
                ShutdownSupport::NotApplicable,
                "{} should name shutdown shape",
                row.capability
            );
        }
        if row.capability == "glommio comparison" {
            assert_eq!(row.status, ReadinessStatus::PlatformGated);
        }
        if row.capability == "Tina-owned io_uring backend" {
            assert_eq!(row.status, ReadinessStatus::NotClaimed);
        }
    }

    app.shutdown().drain().join().expect("matrix app shutdown");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatrixShard(u32);

impl tina::prelude::Shard for MatrixShard {
    fn id(&self) -> tina::prelude::ShardId {
        tina::prelude::ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct MatrixMailboxFactory;

impl tina_runtime::MailboxFactory for MatrixMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn tina::Mailbox<T>> {
        Box::new(MatrixMailbox::new(capacity))
    }
}

struct MatrixMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> MatrixMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> tina::Mailbox<T> for MatrixMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), tina::TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(tina::TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(tina::TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.lock().expect("queue lock").is_empty()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}
