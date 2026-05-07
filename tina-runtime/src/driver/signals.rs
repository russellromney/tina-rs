//! OS signal capture: per-driver SIGINT / SIGTERM flag bits and the
//! parked `signal_wait` entry the driver tracks for each pending call.

use super::*;

/// Captures process-wide OS signals as per-driver flag bits so the
/// runtime step loop can convert them into runtime-owned signal
/// completions.
///
/// Phase 043 Rock 4 contract:
/// * Unix: register `signal-hook` flag handlers for `SIGINT` and
///   `SIGTERM` *per driver*, so every `BetelgeuseDriver` running in a
///   process sees every signal. Registrations are released when the
///   driver is dropped. No async task, no Tokio dependency, no custom
///   unsafe handler — the flag handlers live entirely inside
///   `signal-hook`.
/// * Non-Unix: the dispatcher is a no-op. `signal_wait` calls for
///   "sigint" / "sigterm" still park; they simply never fire from the
///   OS and complete via timeout or cancel only.
///
/// Each `BetelgeuseDriver` owns its own dispatcher so a single SIGINT
/// fans out to every driver's parked `signal_wait` rather than being
/// consumed by whichever driver polls first.
///
/// `signal-hook` chains handlers, so a process that already installed
/// a `SIGINT` handler before constructing a `BetelgeuseDriver` keeps
/// running its handler in addition to ours. The runtime assumes it
/// owns `SIGINT`/`SIGTERM` capture but does not attempt to displace
/// pre-existing handlers.
#[cfg(unix)]
pub(super) struct OsSignalDispatcher {
    pub(super) sigint: Arc<AtomicBool>,
    pub(super) sigterm: Arc<AtomicBool>,
    sigint_token: Option<signal_hook::SigId>,
    sigterm_token: Option<signal_hook::SigId>,
}

#[cfg(not(unix))]
#[derive(Default)]
pub(super) struct OsSignalDispatcher;

#[cfg(unix)]
impl OsSignalDispatcher {
    pub(super) fn install() -> Self {
        let sigint = Arc::new(AtomicBool::new(false));
        let sigterm = Arc::new(AtomicBool::new(false));
        // Signal capture is a Tina capability, not a hidden maybe. If
        // installation fails on a supported platform, fail loudly instead
        // of letting `os_signal_capture_supported()` claim support while
        // no signal can ever arrive.
        let sigint_token =
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&sigint))
                .expect("failed to install Tina SIGINT handler");
        let sigterm_token =
            signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&sigterm))
                .expect("failed to install Tina SIGTERM handler");
        Self {
            sigint,
            sigterm,
            sigint_token: Some(sigint_token),
            sigterm_token: Some(sigterm_token),
        }
    }

    pub(super) fn consume_sigint(&self) -> bool {
        self.sigint.swap(false, Ordering::AcqRel)
    }

    pub(super) fn consume_sigterm(&self) -> bool {
        self.sigterm.swap(false, Ordering::AcqRel)
    }

    /// Test-only constructor that returns a dispatcher whose flags are
    /// not registered with `signal-hook`, so a test can simulate signal
    /// delivery without coupling to process-wide state or interfering
    /// with parallel tests.
    #[cfg(test)]
    pub(super) fn private_for_test() -> Self {
        Self {
            sigint: Arc::new(AtomicBool::new(false)),
            sigterm: Arc::new(AtomicBool::new(false)),
            sigint_token: None,
            sigterm_token: None,
        }
    }
}

#[cfg(unix)]
impl Drop for OsSignalDispatcher {
    fn drop(&mut self) {
        if let Some(token) = self.sigint_token.take() {
            signal_hook::low_level::unregister(token);
        }
        if let Some(token) = self.sigterm_token.take() {
            signal_hook::low_level::unregister(token);
        }
    }
}

#[cfg(not(unix))]
impl OsSignalDispatcher {
    pub(super) fn install() -> Self {
        Self
    }

    pub(super) fn consume_sigint(&self) -> bool {
        false
    }

    pub(super) fn consume_sigterm(&self) -> bool {
        false
    }
}

/// Whether `signal_wait` for "sigint" / "sigterm" is delivered from the
/// host operating system on this build target.
pub const fn os_signal_capture_supported() -> bool {
    cfg!(unix)
}
#[derive(Debug)]
pub(super) struct SignalWaitEntry {
    pub(super) call_id: CallId,
    pub(super) name: String,
    pub(super) deadline: Instant,
    pub(super) cancelled: bool,
}
