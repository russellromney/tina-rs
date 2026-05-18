//! Negative fixture: a bridge classifier must return
//! `BridgeOutcomeClass`, not a string. The four typed arms
//! (`Succeeded`, `Retryable`, `Unavailable`, `Fatal`) are the whole
//! point — if a classifier could hand back `"retryable"` as a string,
//! callers would silently land in retry fog.

use tina_runtime::bridge::BridgeOutcomeClass;

fn classify_thing() -> BridgeOutcomeClass {
    "retryable"
}

fn main() {
    let _ = classify_thing();
}
