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

/// The inventory is file-shaped because the shell guard scans files; the
/// capability report is rail-shaped because users read rails. This is the
/// explicit join between those two stable names.
fn rail_name_for_inventory_path(path: &str) -> &'static str {
    match path {
        "tina-runtime/src/driver/dns.rs" => "dns",
        "tina-runtime/src/driver/process.rs" => "process",
        "tina-runtime/src/driver/storage.rs" => "storage_metadata_fallback",
        other => panic!("unknown inventoried driver path: {other:?}"),
    }
}

#[test]
fn inventory_and_capability_report_agree_on_blocking_lanes() {
    // File inventory classes.
    let inventory = repo_root().join(".intent/runtime-rail-inventory.txt");
    let text = std::fs::read_to_string(&inventory).expect("read rail inventory");
    let inventory_classes: BTreeMap<&'static str, RailClass> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split_whitespace();
            let path = fields.next().expect("inventory line has a path");
            let class = fields.next().expect("inventory line has a classification");
            (rail_name_for_inventory_path(path), class_from_word(class))
        })
        .collect();
    assert!(
        !inventory_classes.is_empty(),
        "inventory should list the current blocking/fallback lanes"
    );

    // Capability report classes that require a justification.
    let caps = RuntimeCapabilities::threaded(4096);
    let report = caps.report();
    let capability_classes: BTreeMap<&'static str, RailClass> = report
        .rows()
        .iter()
        .filter(|row| row.class.requires_justification())
        .map(|row| {
            assert!(
                row.justification.is_some(),
                "rail {} requires a justification but has none",
                row.name
            );
            (row.name, row.class)
        })
        .collect();

    assert_eq!(
        inventory_classes, capability_classes,
        "the file inventory and the capability report disagree on which rails \
         stay blocking/fallback — update both when a rail changes posture",
    );
}
