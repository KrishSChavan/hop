//! OS calls the binding crates do not cover.
//!
//! - Two private CoreGraphics (SkyLight) calls that let a background app
//!   hide the cursor. Synergy, Barrier and Deskflow have relied on them for
//!   years. They are looked up at run time, so a macOS without them still
//!   runs Hop; it just cannot hide the cursor.
//! - `CGSetLocalEventsSuppressionInterval`, deprecated but still the only
//!   way to stop a cursor warp from freezing local input for a moment. It is
//!   looked up at run time too, so a macOS that drops it cannot stop Hop from
//!   launching.
//! - Carbon's `IsSecureEventInputEnabled`, and libSystem's thread
//!   quality-of-service call.

use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::sync::{Once, OnceLock};

use objc2_core_foundation::{CFBoolean, CFString, CFType, kCFBooleanTrue};

/// `RTLD_DEFAULT` from `<dlfcn.h>` on macOS: search every image already
/// loaded into the process.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

unsafe extern "C" {
    /// From libSystem, which every process links.
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    /// From libSystem (`<pthread/qos.h>`): sets the calling thread's
    /// quality-of-service class.
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// `QOS_CLASS_USER_INTERACTIVE` from `<sys/qos.h>`.
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

/// Gives the calling thread the highest quality of service. The event
/// tap's callback holds up every mouse and keyboard event on the Mac until
/// it returns, so its thread must never wait behind background work.
pub(crate) fn make_current_thread_interactive() {
    // SAFETY: the function takes plain numbers and only changes the calling
    // thread's scheduling class. On failure the thread keeps its default
    // class, which still works, just with less headroom.
    let _ = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// HIToolbox (part of Carbon): whether any process has Secure Event
    /// Input switched on. Returns a Carbon `Boolean` (an unsigned char).
    fn IsSecureEventInputEnabled() -> u8;
}

/// Whether macOS Secure Event Input is on, which happens while a password
/// field has keyboard focus. While it is on, event taps see no keystrokes.
pub(crate) fn secure_event_input_enabled() -> bool {
    // SAFETY: the function takes no arguments and only reads system state.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// A WindowServer connection id (`CGSConnectionID`).
type ConnectionId = i32;
/// `CGSConnectionID _CGSDefaultConnection(void)`.
type DefaultConnectionFn = unsafe extern "C" fn() -> ConnectionId;
/// `CGError CGSSetConnectionProperty(CGSConnectionID, CGSConnectionID,
/// CFStringRef key, CFTypeRef value)`.
type SetConnectionPropertyFn =
    unsafe extern "C" fn(ConnectionId, ConnectionId, *const CFString, *const CFType) -> i32;
/// `CGError CGSetLocalEventsSuppressionInterval(CFTimeInterval)`.
type SetSuppressionIntervalFn = unsafe extern "C" fn(f64) -> i32;

/// Finds a C function exported by a loaded framework.
fn lookup(name: &CStr) -> Option<NonNull<c_void>> {
    // SAFETY: `name` is NUL-terminated, and with `RTLD_DEFAULT` dlsym only
    // reads the symbol tables of images that are already loaded.
    NonNull::new(unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) })
}

/// The private calls behind cursor hiding.
struct Cgs {
    default_connection: DefaultConnectionFn,
    set_connection_property: SetConnectionPropertyFn,
}

fn cgs() -> Option<&'static Cgs> {
    static CGS: OnceLock<Option<Cgs>> = OnceLock::new();
    CGS.get_or_init(|| {
        let default_connection = lookup(c"_CGSDefaultConnection")?;
        let set_connection_property = lookup(c"CGSSetConnectionProperty")?;
        // SAFETY: the symbol is a C function with exactly this signature (it
        // lives in SkyLight, and CoreGraphics re-exports it), and a function
        // pointer has the same size as a data pointer on macOS.
        let default_connection = unsafe {
            std::mem::transmute::<*mut c_void, DefaultConnectionFn>(default_connection.as_ptr())
        };
        // SAFETY: as above, for the other function.
        let set_connection_property = unsafe {
            std::mem::transmute::<*mut c_void, SetConnectionPropertyFn>(
                set_connection_property.as_ptr(),
            )
        };
        Some(Cgs {
            default_connection,
            set_connection_property,
        })
    })
    .as_ref()
}

/// Lets this process hide the cursor even when it is not the active app,
/// which a menu-bar app like Hop never is while the user works elsewhere.
///
/// Returns `false` if the private calls are missing or refuse; the caller
/// then skips hiding, because `CGDisplayHideCursor` would do nothing anyway.
pub(crate) fn allow_cursor_hiding_in_background() -> bool {
    let Some(cgs) = cgs() else {
        return false;
    };
    // SAFETY: reading an immutable CoreFoundation constant.
    let Some(yes) = (unsafe { kCFBooleanTrue }) else {
        return false;
    };
    let key = CFString::from_static_str("SetsCursorInBackground");
    let key_ptr: *const CFString = &*key;
    let value_ptr: *const CFType = (yes as *const CFBoolean).cast();
    // SAFETY: the pointer came from `cgs()` with the right signature; the
    // function takes no arguments and returns this process's connection id.
    let connection = unsafe { (cgs.default_connection)() };
    // SAFETY: the pointer came from `cgs()` with the right signature; `key`
    // and `yes` are valid CF objects that outlive the call, and the function
    // retains whatever it keeps.
    let error =
        unsafe { (cgs.set_connection_property)(connection, connection, key_ptr, value_ptr) };
    error == 0
}

/// Stops cursor warps from freezing local mouse input for about a quarter
/// of a second, which macOS does by default after `CGWarpMouseCursorPosition`.
/// Without this, the first motion after parking or restoring the cursor
/// would be lost. Done once per process.
pub(crate) fn allow_input_right_after_warps() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(function) = lookup(c"CGSetLocalEventsSuppressionInterval") else {
            return;
        };
        // SAFETY: the symbol is CoreGraphics' C function with this signature,
        // and a function pointer has the same size as a data pointer.
        let function = unsafe {
            std::mem::transmute::<*mut c_void, SetSuppressionIntervalFn>(function.as_ptr())
        };
        // SAFETY: the function takes a plain number and changes only this
        // process's own suppression setting. Its error code is ignored: on
        // failure warps just keep the default short pause.
        let _ = unsafe { function(0.0) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_symbols_resolve_on_this_mac() {
        // Looking the symbols up calls nothing, so this is safe to run.
        assert!(lookup(c"_CGSDefaultConnection").is_some());
        assert!(lookup(c"CGSSetConnectionProperty").is_some());
        assert!(lookup(c"CGSetLocalEventsSuppressionInterval").is_some());
        assert!(lookup(c"HopNoSuchFunction").is_none());
    }
}
