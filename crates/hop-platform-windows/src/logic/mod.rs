//! The backend's pure logic: coordinate conversion, motion and edge-push
//! tracking, key and wheel rules, the watchdog's decisions, the veil over a
//! cursor left behind, clipboard text, and the drop zones.
//!
//! Nothing in here calls Windows, so all of it is unit-tested, and the rules
//! the OS-facing modules follow can be read in one place. The OS-facing
//! modules feed these functions what Windows reports and do what they return.

pub mod clipboard;
pub mod coords;
pub mod displays;
pub mod drop_zone;
pub mod keys;
pub mod motion;
pub mod park;
pub mod scroll;
pub mod swallow;
pub mod veil;
pub mod watchdog;
