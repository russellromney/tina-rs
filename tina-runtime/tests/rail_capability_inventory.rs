//! Capability/inventory cross-check.
//!
//! Two sources describe Tina's runtime-owned rails, and they must agree:
//!
//! - `.intent/runtime-rail-inventory.txt` is the file-level list of rails that
//!   are NOT substrate-backed (worker threads / blocking std socket/file work),
//!   policed by `scripts/rail_inventory_guard.sh`.
//! - The runtime capability report (`RuntimeCapabilities::report`) classifies
//!   every rail with a [`RailClass`].
//!
//! This test proves the two never drift: the file inventory's classifications
//! (`fallback-worker` / `justified-blocking-lane`) match, one-for-one, the
//! capability rails that carry a justification class. A rail added to one
//! source but not the other fails here, in addition to the shell guard that
//! ties the inventory to the actual code.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tina_runtime::{RailClass, RuntimeCapabilities};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tina-runtime has a parent")
        .to_path_buf()
}

/// Maps an inventory classification word to a [`RailClass`].
fn class_from_word(word: &str) -> RailClass {
    match word {
        "fallback-worker" => RailClass::FallbackWorker,
        "justified-blocking-lane" => RailClass::JustifiedBlockingLane,
        other => panic!("unknown inventory classification: {other:?}"),
    }
}

/// Tally of how many inventory/capability entries carry each class.
fn tally(classes: impl IntoIterator<Item = RailClass>) -> BTreeMap<&'static str, usize> {
    let mut map = BTreeMap::new();
    for class in classes {
        let key = match class {
            RailClass::FallbackWorker => "fallback-worker",
            RailClass::JustifiedBlockingLane => "justified-blocking-lane",
            other => panic!("unexpected justification-carrying class {other:?}"),
        };
        *map.entry(key).or_insert(0) += 1;
    }
    map
}

#[test]
fn inventory_and_capability_report_agree_on_blocking_lanes() {
    // File inventory classes.
    let inventory = repo_root().join(".intent/runtime-rail-inventory.txt");
    let text = std::fs::read_to_string(&inventory).expect("read rail inventory");
    let inventory_classes: Vec<RailClass> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split_whitespace();
            let _path = fields.next().expect("inventory line has a path");
            let class = fields.next().expect("inventory line has a classification");
            class_from_word(class)
        })
        .collect();
    assert!(
        !inventory_classes.is_empty(),
        "inventory should list the current blocking/fallback lanes"
    );

    // Capability report classes that require a justification.
    let caps = RuntimeCapabilities::threaded(4096);
    let report = caps.report();
    let capability_classes: Vec<RailClass> = report
        .rows()
        .iter()
        .filter(|row| row.class.requires_justification())
        .map(|row| {
            assert!(
                row.justification.is_some(),
                "rail {} requires a justification but has none",
                row.name
            );
            row.class
        })
        .collect();

    assert_eq!(
        tally(inventory_classes),
        tally(capability_classes),
        "the file inventory and the capability report disagree on the set of \
         blocking/fallback lanes — update both when a rail changes posture",
    );
}
