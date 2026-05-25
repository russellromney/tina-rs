//! Hard CPU affinity for shard worker threads.
//!
//! `configured_core = Some(n)` means "pin this shard worker to OS CPU id `n` if
//! the platform can." `n` is an OS CPU id checked against the process's allowed
//! affinity mask, **not** an index into `0..num_cpus`: containers and cpusets
//! can expose sparse allowed ids, so we never assume CPU 0 exists or that ids
//! are dense.
//!
//! - **Linux** pins via `sched_setaffinity` to a single-core mask and reads the
//!   running core back with `sched_getcpu`, proving the pin instead of guessing.
//!   A requested core outside the allowed mask is refused with a typed
//!   [`AffinityStatus::Failed`] — never a silent mis-pin to some other core.
//! - **macOS and every other platform** offer only affinity *hints*, not a hard
//!   pin, so they report [`AffinityStatus::Unsupported`] and run unpinned.
//!
//! The pin runs from inside the worker thread itself (affinity is per-thread).
//! Only the shard worker is pinned. A pinned thread's children inherit its
//! single-core mask on Linux, so helper-lane threads the worker spawns later
//! (e.g. per-operation TLS workers) call [`float_helper_thread`] at startup to
//! reset to the process's original mask — helper lanes stay unpinned and float
//! onto spare cores rather than fighting a shard for its core.

#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use crate::live_report::AffinityStatus;

/// The process's original allowed-CPU mask, captured before any shard worker
/// narrows its own affinity. Used to float helper-lane threads back off a
/// worker's single-core pin. Process-wide and stable, so a set-once cell fits.
#[cfg(target_os = "linux")]
static ORIGINAL_ALLOWED: OnceLock<Vec<usize>> = OnceLock::new();

/// Result of an affinity attempt: the status to publish and the core the worker
/// was observed running on (only known on a proven Linux pin).
pub(crate) struct AffinityOutcome {
    pub(crate) status: AffinityStatus,
    pub(crate) observed_core: Option<usize>,
}

impl AffinityOutcome {
    fn not_requested() -> Self {
        Self {
            status: AffinityStatus::NotRequested,
            observed_core: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn applied(observed_core: usize) -> Self {
        Self {
            status: AffinityStatus::Applied,
            observed_core: Some(observed_core),
        }
    }

    #[cfg(target_os = "linux")]
    fn failed(reason: String) -> Self {
        Self {
            status: AffinityStatus::Failed(reason),
            observed_core: None,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn unsupported() -> Self {
        Self {
            status: AffinityStatus::Unsupported,
            observed_core: None,
        }
    }
}

/// Applies the configured pin from *inside* the calling (worker) thread.
///
/// `None` makes no affinity syscall and reports `NotRequested`.
pub(crate) fn apply(configured_core: Option<usize>) -> AffinityOutcome {
    match configured_core {
        None => AffinityOutcome::not_requested(),
        Some(core) => apply_core(core),
    }
}

/// Resets the calling thread's CPU affinity to the process's original allowed
/// mask, if some shard worker has pinned itself.
///
/// A pinned worker's child threads inherit its single-core mask on Linux. Helper
/// lanes call this at startup so they float across spare cores instead of being
/// stuck on a shard's core. Best-effort and a no-op when nothing has pinned or
/// the platform has no hard pin.
#[cfg(target_os = "linux")]
pub(crate) fn float_helper_thread() {
    if let Some(allowed) = ORIGINAL_ALLOWED.get() {
        let _ = set_affinity(allowed);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn float_helper_thread() {}

/// Rejects a requested core that is not in the process's allowed affinity mask.
///
/// Pure and platform-independent so the reject path is unit-testable without
/// syscalls; the Linux pin feeds it the mask read from `sched_getaffinity`.
#[cfg(any(target_os = "linux", test))]
fn validate_core(core: usize, allowed: &[usize]) -> Result<(), String> {
    if allowed.contains(&core) {
        Ok(())
    } else {
        Err(format!(
            "core {core} is not in the process affinity mask {allowed:?}"
        ))
    }
}

#[cfg(target_os = "linux")]
fn apply_core(core: usize) -> AffinityOutcome {
    match pin_current_thread(core) {
        Ok(observed_core) => AffinityOutcome::applied(observed_core),
        Err(reason) => AffinityOutcome::failed(reason),
    }
}

/// Pins the calling thread to `core` and proves it by reading the running core
/// back. A single-core mask makes `sched_getcpu` deterministic on a multi-core
/// box, so the readback is not flaky.
#[cfg(target_os = "linux")]
fn pin_current_thread(core: usize) -> Result<usize, String> {
    let allowed = read_allowed_cores()?;
    // Remember the full mask before we narrow ours so helper threads this worker
    // later spawns can float back to it. Set-once; identical across shards.
    ORIGINAL_ALLOWED.get_or_init(|| allowed.clone());
    validate_core(core, &allowed)?;
    set_affinity(&[core])?;
    let observed = read_current_core()?;
    if observed != core {
        return Err(format!(
            "requested core {core} but sched_getcpu reports {observed} after pinning"
        ));
    }
    Ok(observed)
}

/// Reads the calling thread's allowed-CPU mask into a dense list of ids.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn read_allowed_cores() -> Result<Vec<usize>, String> {
    // SAFETY: a zeroed `cpu_set_t` is a valid empty set used as an out-param.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: pid 0 = the calling thread; `set` is a valid, correctly sized
    // out-parameter for the mask.
    let rc =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 {
        return Err(format!(
            "sched_getaffinity failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut cores = Vec::new();
    for cpu in 0..(libc::CPU_SETSIZE as usize) {
        // SAFETY: `cpu` is below CPU_SETSIZE and `set` was filled by
        // sched_getaffinity above.
        if unsafe { libc::CPU_ISSET(cpu, &set) } {
            cores.push(cpu);
        }
    }
    Ok(cores)
}

/// Sets the calling thread's affinity to exactly `cores`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn set_affinity(cores: &[usize]) -> Result<(), String> {
    // SAFETY: a zeroed `cpu_set_t` is a valid empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &cpu in cores {
        // Ids come from a read affinity mask, so they are below CPU_SETSIZE; the
        // guard keeps `CPU_SET` in bounds regardless.
        if cpu < libc::CPU_SETSIZE as usize {
            // SAFETY: `cpu` is below CPU_SETSIZE and `set` is initialised.
            unsafe { libc::CPU_SET(cpu, &mut set) };
        }
    }
    // SAFETY: pid 0 = the calling thread; `set` is a valid mask.
    let rc = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if rc != 0 {
        return Err(format!(
            "sched_setaffinity failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Reads the core the calling thread is currently running on.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn read_current_core() -> Result<usize, String> {
    // SAFETY: `sched_getcpu` takes no arguments and has no preconditions.
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 {
        return Err(format!(
            "sched_getcpu failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(cpu as usize)
}

#[cfg(not(target_os = "linux"))]
fn apply_core(_core: usize) -> AffinityOutcome {
    // macOS and other platforms expose only affinity *hints*, not a hard pin.
    // We refuse to dress a hint up as a pin: report Unsupported, run unpinned.
    AffinityOutcome::unsupported()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_core_rejects_a_core_outside_the_mask() {
        let err =
            validate_core(7, &[0, 1, 2]).expect_err("a core outside the mask must be rejected");
        assert!(err.contains('7'), "reason names the requested core: {err}");
        assert!(err.contains("affinity mask"), "reason is typed: {err}");
    }

    #[test]
    fn validate_core_accepts_a_core_in_the_mask() {
        validate_core(2, &[0, 1, 2]).expect("a core in the mask is allowed");
    }

    #[test]
    fn apply_none_is_not_requested_and_makes_no_pin() {
        let outcome = apply(None);
        assert_eq!(outcome.status, AffinityStatus::NotRequested);
        assert_eq!(outcome.observed_core, None);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn apply_some_reports_unsupported_off_linux() {
        // No assumption that CPU 0 exists: the platform has no hard pin at all,
        // so any requested id reports Unsupported and stays unpinned.
        let outcome = apply(Some(0));
        assert_eq!(outcome.status, AffinityStatus::Unsupported);
        assert_eq!(outcome.observed_core, None);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn float_helper_thread_is_a_noop_off_linux() {
        // Must compile and be callable everywhere; no hard pin means nothing to
        // float back from.
        float_helper_thread();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_pins_to_an_allowed_core_and_observes_it() {
        let allowed = read_allowed_cores().expect("read the process affinity mask");
        // Choose a real allowed id rather than assuming CPU 0 is present.
        let core = *allowed
            .first()
            .expect("the process has at least one allowed core");
        let outcome = apply(Some(core));
        assert_eq!(outcome.status, AffinityStatus::Applied);
        assert_eq!(outcome.observed_core, Some(core));
        // Restore this thread so we do not narrow other tests sharing it.
        float_helper_thread();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_reports_failed_for_a_core_outside_the_mask() {
        let allowed = read_allowed_cores().expect("read the process affinity mask");
        let max = allowed.iter().copied().max().unwrap_or(0);
        // First id not in the allowed mask; works for sparse masks too.
        let absent = (0..=max + 1)
            .find(|cpu| !allowed.contains(cpu))
            .expect("an unallowed core id exists at or below max+1");
        let outcome = apply(Some(absent));
        assert!(
            matches!(outcome.status, AffinityStatus::Failed(_)),
            "unavailable core must fail loudly, got {:?}",
            outcome.status
        );
        assert_eq!(outcome.observed_core, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn float_helper_thread_unpins_a_child_spawned_under_a_pin() {
        // Pin this thread, then prove a thread spawned under the pin inherits the
        // single-core mask and that float_helper_thread restores the original
        // mask — the mechanism that keeps helper lanes off a shard's core.
        let here = read_allowed_cores().expect("read the process affinity mask");
        let core = *here.first().expect("an allowed core");
        assert_eq!(apply(Some(core)).status, AffinityStatus::Applied);
        let original = ORIGINAL_ALLOWED
            .get()
            .cloned()
            .expect("apply captured the original mask");

        let (inherited, floated) = std::thread::spawn(|| {
            let inherited = read_allowed_cores().expect("child reads its inherited mask");
            float_helper_thread();
            let floated = read_allowed_cores().expect("child reads its floated mask");
            (inherited, floated)
        })
        .join()
        .expect("child thread joins");

        assert_eq!(
            inherited,
            vec![core],
            "a thread spawned under the pin inherits the single-core mask"
        );
        assert_eq!(
            floated, original,
            "float_helper_thread restores the original allowed mask"
        );

        // Restore this thread so we do not narrow other tests sharing it.
        float_helper_thread();
    }
}
