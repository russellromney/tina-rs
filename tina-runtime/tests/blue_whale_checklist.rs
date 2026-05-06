#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlueWhaleStatus {
    True,
    Partial,
    Advisory,
    Future,
    NonGoal,
}

#[derive(Debug, Clone, Copy)]
struct BlueWhaleItem {
    name: &'static str,
    status: BlueWhaleStatus,
    evidence: &'static str,
}

const BLUE_WHALE_CHECKLIST: &[BlueWhaleItem] = &[
    BlueWhaleItem {
        name: "per-core ownership",
        status: BlueWhaleStatus::Partial,
        evidence: "one live worker owns one shard runtime; topology reports shard, worker name, thread id, configured core",
    },
    BlueWhaleItem {
        name: "thread pinning",
        status: BlueWhaleStatus::Advisory,
        evidence: "AffinityStatus reports NotRequested or AdvisoryOnly; no OS pinning claim yet",
    },
    BlueWhaleItem {
        name: "SPSC/cross-shard queues",
        status: BlueWhaleStatus::True,
        evidence: "bounded mailbox and bounded live source-target shard queues reject Full/Closed visibly",
    },
    BlueWhaleItem {
        name: "bounded queues",
        status: BlueWhaleStatus::True,
        evidence: "LocalSystemConfig rejects zero capacities and topology reports lane capacities/pressure",
    },
    BlueWhaleItem {
        name: "preallocation",
        status: BlueWhaleStatus::Partial,
        evidence: "PreallocationConfig reserves runtime-owned metadata; user payloads and backend slots remain out of scope",
    },
    BlueWhaleItem {
        name: "allocator locality",
        status: BlueWhaleStatus::Future,
        evidence: "no per-shard allocator or NUMA allocator policy yet",
    },
    BlueWhaleItem {
        name: "polling model",
        status: BlueWhaleStatus::Partial,
        evidence: "worker loop explicitly advances the Tina runtime and driver; backend polling details remain substrate-specific",
    },
    BlueWhaleItem {
        name: "network/storage backend",
        status: BlueWhaleStatus::Partial,
        evidence: "portable Betelgeuse-backed driver exists; io_uring backend is North Sea future work",
    },
    BlueWhaleItem {
        name: "NUMA",
        status: BlueWhaleStatus::NonGoal,
        evidence: "Blue Whale explicitly refuses NUMA policy",
    },
    BlueWhaleItem {
        name: "scheduler groups",
        status: BlueWhaleStatus::Future,
        evidence: "cooperative turn fairness is pinned; service-class weights are not in this phase",
    },
    BlueWhaleItem {
        name: "DST/replay",
        status: BlueWhaleStatus::True,
        evidence: "tina-sim owns deterministic replay/DST; Blue Whale adds combined e2e pressure over live reports",
    },
    BlueWhaleItem {
        name: "non-await user model",
        status: BlueWhaleStatus::True,
        evidence: "isolates return effects; no raw io_uring/Tokio/await surface leaks into handlers",
    },
];

#[test]
fn blue_whale_checklist_classifies_every_seastar_shaped_claim() {
    assert_eq!(BLUE_WHALE_CHECKLIST.len(), 12);
    for item in BLUE_WHALE_CHECKLIST {
        assert!(!item.name.is_empty());
        assert!(
            !item.evidence.is_empty(),
            "missing evidence for {}",
            item.name
        );
    }

    assert!(
        BLUE_WHALE_CHECKLIST.iter().any(|item| {
            item.name == "thread pinning" && item.status == BlueWhaleStatus::Advisory
        })
    );
    assert!(
        BLUE_WHALE_CHECKLIST
            .iter()
            .any(|item| { item.name == "NUMA" && item.status == BlueWhaleStatus::NonGoal })
    );
    assert!(BLUE_WHALE_CHECKLIST.iter().any(|item| {
        item.name == "network/storage backend" && item.status == BlueWhaleStatus::Partial
    }));
}
