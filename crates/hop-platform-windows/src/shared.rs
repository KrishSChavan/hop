//! State shared between the caller's threads and Hop's input thread: the
//! suppression lease, and which capture handle owns the hooks.
//!
//! The lease's bookkeeping is `hop_platform::SuppressLease` behind a mutex,
//! changed by `set_mode` and the watchdog. The hook callbacks, which run on
//! every input event and must never wait, read a lock-free copy of its
//! expiry instead.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use hop_platform::{CaptureMode, SUPPRESS_LEASE, SuppressLease};

use crate::logic::watchdog;

/// The lease, for `set_mode` and the watchdog.
static LEASE: Mutex<Option<SuppressLease>> = Mutex::new(None);

/// When the lease expires, as [`encode`]d time; 0 for no lease. Written
/// only while holding [`LEASE`], read by the hooks without it.
static EXPIRES: AtomicU64 = AtomicU64::new(0);

/// Set while a lease that ran out has not been cleaned up yet and the input
/// thread has been asked to do it (see [`lapse_needs_cleanup`]).
static LAPSE_REPORTED: AtomicBool = AtomicBool::new(false);

static CAPTURE_CLAIMED: AtomicBool = AtomicBool::new(false);

/// The zero point for [`encode`].
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// `t` as nanoseconds since [`EPOCH`], plus one so that 0 can mean "none".
/// Order-preserving, which is all the comparisons need.
fn encode(t: Instant) -> u64 {
    let nanos = t.saturating_duration_since(*EPOCH).as_nanos();
    u64::try_from(nanos)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// The lease, even if a thread panicked while holding it: it is plain data
/// that is valid at every step, so it is safe to keep using.
fn lease() -> MutexGuard<'static, Option<SuppressLease>> {
    LEASE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether physical input is being swallowed at `now`. Lock-free.
pub(crate) fn suppressing(now: Instant) -> bool {
    let expires = EXPIRES.load(Ordering::Acquire);
    expires != 0 && encode(now) < expires
}

/// Whether the lease has run out at `now` but the watchdog has not cleaned
/// up yet, and nobody has asked it to. Returns true once per lapse, so the
/// caller can ask the input thread to clean up right away instead of at the
/// next tick. Lock-free.
pub(crate) fn lapse_needs_cleanup(now: Instant) -> bool {
    let expires = EXPIRES.load(Ordering::Acquire);
    expires != 0 && encode(now) >= expires && !LAPSE_REPORTED.swap(true, Ordering::AcqRel)
}

/// `CaptureMode::Suppress` starts or renews the lease; `Passive` ends it.
pub(crate) fn set_mode(mode: CaptureMode, now: Instant) {
    let mut lease = lease();
    let lease = lease.get_or_insert_with(SuppressLease::default);
    match mode {
        CaptureMode::Suppress => {
            lease.renew(now);
            LAPSE_REPORTED.store(false, Ordering::Release);
            EXPIRES.store(encode(now + SUPPRESS_LEASE), Ordering::Release);
        }
        CaptureMode::Passive => {
            lease.clear();
            EXPIRES.store(0, Ordering::Release);
        }
    }
}

/// The mode in effect at `now`, after any lapse.
pub(crate) fn mode(now: Instant) -> CaptureMode {
    if suppressing(now) {
        CaptureMode::Suppress
    } else {
        CaptureMode::Passive
    }
}

/// The watchdog's check: true once when a renewed lease has run out, which
/// also clears it.
pub(crate) fn lease_lapsed(now: Instant) -> bool {
    let mut lease = lease();
    let Some(lease) = lease.as_mut() else {
        return false;
    };
    let lapsed = watchdog::lease_lapsed(lease, now);
    if lapsed {
        EXPIRES.store(0, Ordering::Release);
    }
    LAPSE_REPORTED.store(false, Ordering::Release);
    lapsed
}

/// Ends suppression at once, whatever state the lease was in.
pub(crate) fn end_suppression() {
    // The guard is held until the end, so EXPIRES is written under the lock.
    let mut lease = lease();
    if let Some(lease) = lease.as_mut() {
        lease.clear();
    }
    EXPIRES.store(0, Ordering::Release);
}

/// Claims the hooks for one capture handle. Windows delivers low-level hook
/// events to the thread that installed them, and the callbacks report to a
/// single sink, so only one capture can run per process.
pub(crate) fn claim_capture() -> bool {
    CAPTURE_CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Gives the hooks back.
pub(crate) fn release_capture() {
    CAPTURE_CLAIMED.store(false, Ordering::Release);
}
