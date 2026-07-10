//! Regression test for `scripts/rail_inventory_guard.sh`.
//!
//! The guard is a load-bearing CI check: it forces a human to classify a rail
//! and update the runtime capability report before a new worker thread,
//! blocking std socket, or blocking `std::fs` call lands in a runtime-owned
//! rail. So it needs its own proof that it actually fires. This drives the
//! real script against temp fixtures via its `RAIL_GUARD_SRC_DIRS` /
//! `RAIL_INVENTORY` overrides and checks that it:
//!
//! - passes when the inventory matches the bypass surface,
//! - fails on a new off-inventory bypass primitive (a worker thread),
//! - fails on a stale inventory entry,
//! - ignores comment-only matches, block comments, and in-`src` `tests.rs`
//!   modules.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/tina-runtime.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tina-runtime has a parent")
        .to_path_buf()
}

fn unique_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rail_guard_{}_{}_{}", tag, std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Runs the real guard against a fixture driver dir + inventory. Returns true
/// on a clean (exit 0) result.
fn guard_passes(driver_dir: &Path, inventory: &Path) -> bool {
    let script = repo_root().join("scripts/rail_inventory_guard.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env("RAIL_GUARD_SRC_DIRS", driver_dir)
        .env("RAIL_INVENTORY", inventory)
        .output()
        .expect("run rail_inventory_guard.sh");
    out.status.success()
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture file");
}

#[test]
fn guard_passes_when_inventory_matches_surface() {
    let fx = unique_dir("clean");
    let driver = fx.join("driver");
    write(
        &driver.join("dns.rs"),
        "use std::thread;\nfn lane() { thread::spawn(|| {}); }\n",
    );
    let inventory = fx.join("inventory.txt");
    write(
        &inventory,
        &format!(
            "# header\n{}/dns.rs  justified-blocking-lane  resolver worker\n",
            driver.display()
        ),
    );

    assert!(
        guard_passes(&driver, &inventory),
        "guard should pass when the bypass surface matches the inventory"
    );
}

#[test]
fn guard_fails_on_new_offlist_bypass_primitive() {
    let fx = unique_dir("new");
    let driver = fx.join("driver");
    write(
        &driver.join("dns.rs"),
        "use std::thread;\nfn lane() { thread::spawn(|| {}); }\n",
    );
    // A second rail that spawns a worker thread but is NOT inventoried.
    write(
        &driver.join("sneaky.rs"),
        "use std::thread;\nfn hidden() { thread::spawn(|| {}); }\n",
    );
    let inventory = fx.join("inventory.txt");
    write(
        &inventory,
        &format!(
            "{}/dns.rs  justified-blocking-lane  resolver worker\n",
            driver.display()
        ),
    );

    assert!(
        !guard_passes(&driver, &inventory),
        "guard must fail when a new bypass primitive appears off the inventory"
    );
}

#[test]
fn guard_fails_on_new_blocking_unix_socket() {
    let fx = unique_dir("unix");
    let driver = fx.join("driver");
    // Regression: a hidden std blocking Unix-domain socket lane that is
    // not inventoried must be rejected.
    write(
        &driver.join("unix.rs"),
        "use std::os::unix::net::UnixListener;\nfn lane() { let _ = UnixListener::bind(\"/tmp/x\"); }\n",
    );
    let inventory = fx.join("inventory.txt");
    write(&inventory, "# all rails ride the substrate\n");

    assert!(
        !guard_passes(&driver, &inventory),
        "guard must fail when a blocking std Unix-domain socket reappears off-inventory"
    );
}

#[test]
fn guard_fails_on_stale_inventory_entry() {
    let fx = unique_dir("stale");
    let driver = fx.join("driver");
    write(
        &driver.join("dns.rs"),
        "use std::thread;\nfn lane() { thread::spawn(|| {}); }\n",
    );
    let inventory = fx.join("inventory.txt");
    write(
        &inventory,
        &format!(
            "{}/dns.rs  justified-blocking-lane  resolver\n{}/gone.rs  fallback-worker  moved\n",
            driver.display(),
            driver.display()
        ),
    );

    assert!(
        !guard_passes(&driver, &inventory),
        "guard must fail when an inventory entry no longer has a bypass primitive"
    );
}

#[test]
fn guard_ignores_comment_only_and_test_modules() {
    let fx = unique_dir("ignore");
    let driver = fx.join("driver");
    // Only a doc comment names a worker thread — not real usage.
    write(
        &driver.join("doc_only.rs"),
        "/// This rail used to use thread::spawn before it rode the substrate.\npub fn f() {}\n",
    );
    // A block-comment body naming a bypass primitive should also stay prose,
    // not become an inventory requirement.
    write(
        &driver.join("block_comment.rs"),
        "/*\n * Old implementation used std::os::unix::net here.\n */\npub fn f() {}\n",
    );
    // A real primitive, but in an in-src test module (not shipped surface).
    write(
        &driver.join("tests.rs"),
        "use std::thread;\nfn t() { thread::spawn(|| {}); }\n",
    );
    let inventory = fx.join("inventory.txt");
    write(&inventory, "# nothing real here\n");

    assert!(
        guard_passes(&driver, &inventory),
        "comment-only matches and tests.rs must not count as bypass surface"
    );
}
