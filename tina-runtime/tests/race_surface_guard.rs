//! Regression test for `scripts/race_surface_guard.sh`.
//!
//! The guard is a load-bearing CI check (it forces review + a model before a
//! new synchronization primitive lands), so it needs its own proof that it
//! actually fires. This drives the real script against temp fixtures via its
//! `RACE_GUARD_SRC_DIRS` / `RACE_GUARD_ALLOWLIST` overrides and checks that it:
//!
//! - passes when the allowlist matches the surface,
//! - fails on a new off-list primitive,
//! - fails on a stale allowlist entry,
//! - ignores comment-only matches and in-`src` `tests.rs` modules.

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
    let dir = std::env::temp_dir().join(format!("race_guard_{}_{}_{}", tag, std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Runs the real guard against a fixture `src` dir + allowlist. Returns true
/// on a clean (exit 0) result.
fn guard_passes(src_dir: &Path, allowlist: &Path) -> bool {
    let script = repo_root().join("scripts/race_surface_guard.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env("RACE_GUARD_SRC_DIRS", src_dir)
        .env("RACE_GUARD_ALLOWLIST", allowlist)
        .output()
        .expect("run race_surface_guard.sh");
    out.status.success()
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture file");
}

#[test]
fn guard_passes_when_allowlist_matches_surface() {
    let fx = unique_dir("clean");
    let src = fx.join("src");
    write(
        &src.join("scope.rs"),
        "use std::sync::atomic::AtomicU64;\npub static C: AtomicU64 = AtomicU64::new(0);\n",
    );
    let allowlist = fx.join("allow.txt");
    write(
        &allowlist,
        &format!("# header\n{}/scope.rs  id-counter\n", src.display()),
    );

    assert!(
        guard_passes(&src, &allowlist),
        "guard should pass when the surface matches the allowlist"
    );
}

#[test]
fn guard_fails_on_new_offlist_primitive() {
    let fx = unique_dir("new");
    let src = fx.join("src");
    write(
        &src.join("scope.rs"),
        "use std::sync::atomic::AtomicU64;\npub static C: AtomicU64 = AtomicU64::new(0);\n",
    );
    // A second file with a primitive that is NOT on the allowlist.
    write(
        &src.join("sneaky.rs"),
        "use std::cell::UnsafeCell;\npub struct S(UnsafeCell<u8>);\n",
    );
    let allowlist = fx.join("allow.txt");
    write(
        &allowlist,
        &format!("{}/scope.rs  id-counter\n", src.display()),
    );

    assert!(
        !guard_passes(&src, &allowlist),
        "guard must fail when a new primitive appears off the allowlist"
    );
}

#[test]
fn guard_fails_on_stale_allowlist_entry() {
    let fx = unique_dir("stale");
    let src = fx.join("src");
    write(
        &src.join("scope.rs"),
        "use std::sync::atomic::AtomicU64;\npub static C: AtomicU64 = AtomicU64::new(0);\n",
    );
    let allowlist = fx.join("allow.txt");
    write(
        &allowlist,
        &format!(
            "{}/scope.rs  id-counter\n{}/gone.rs  id-counter\n",
            src.display(),
            src.display()
        ),
    );

    assert!(
        !guard_passes(&src, &allowlist),
        "guard must fail when an allowlist entry no longer has a primitive"
    );
}

#[test]
fn guard_ignores_comment_only_and_test_modules() {
    let fx = unique_dir("ignore");
    let src = fx.join("src");
    // Only a doc comment names an atomic — not real usage.
    write(
        &src.join("doc_only.rs"),
        "/// Replaces `Arc<AtomicBool>` done flags in user code.\npub fn f() {}\n",
    );
    // A real primitive, but in an in-src test module (not shipped surface).
    write(
        &src.join("tests.rs"),
        "use std::sync::atomic::AtomicBool;\nstatic T: AtomicBool = AtomicBool::new(false);\n",
    );
    // Empty allowlist: if either file were counted, the guard would fail.
    let allowlist = fx.join("allow.txt");
    write(&allowlist, "# nothing real here\n");

    assert!(
        guard_passes(&src, &allowlist),
        "comment-only matches and tests.rs must not count as surface"
    );
}
