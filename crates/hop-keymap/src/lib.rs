//! Key tables, the Cmd/Ctrl swap and pressed-key tracking for Hop. See
//! docs/ARCHITECTURE.md ("Keyboard").
//!
//! Keys travel between devices as USB HID usages ([`KeyCode`]), which name
//! physical key positions, not characters: the receiving OS applies its own
//! keyboard layout. This crate:
//!
//! - converts between HID usages and each OS's native codes: [`from_mac`] and
//!   [`to_mac`] for macOS virtual keycodes (`kVK_*`), and
//!   [`from_windows_scancode`] and [`to_windows_scancode`] for Windows Set 1
//!   scancodes with the extended (E0) flag;
//! - names keys the way the W3C UI Events spec does (`"KeyA"`, `"MetaLeft"`,
//!   `"Escape"`), for the config file: [`name`] and [`from_name`];
//! - swaps Command and Control between a Mac and a PC, when that setting is
//!   on (it is off by default): [`ModifierPolicy`];
//! - remembers which key it injected for each held key, so every key-up
//!   matches its key-down and a session can end with nothing stuck down:
//!   [`Translator`], and [`ButtonTracker`] for mouse buttons.
//!
//! The crate is pure logic: it makes no OS calls.
//!
//! # Example
//!
//! Cmd+C typed on a Mac and injected on Windows, first as it is by default,
//! then with the Cmd/Ctrl swap on:
//!
//! ```
//! use hop_keymap::{ModifierPolicy, Translator, from_mac, to_windows_scancode};
//! use hop_types::Os;
//!
//! // The Mac turns its keycodes (kVK_Command, kVK_ANSI_C) into HID usages...
//! let keys_typed: Vec<_> = [from_mac(0x37), from_mac(0x08)]
//!     .into_iter()
//!     .flatten()
//!     .collect();
//!
//! // ...and Windows looks up scancodes. By default the keys arrive as
//! // labelled: the Windows key (an extended key, E0 5B) and C.
//! let mut keys = Translator::new(ModifierPolicy::default(), Os::MacOs, Os::Windows);
//! let injected: Vec<_> = keys_typed
//!     .iter()
//!     .map(|&key| to_windows_scancode(keys.press(key)))
//!     .collect();
//! assert_eq!(injected, [Some((0x5B, true)), Some((0x2E, false))]);
//!
//! // With the swap on, Windows swaps Command for Control first: Left
//! // Control and C, neither of them an extended key.
//! let swap = ModifierPolicy { swap_cmd_ctrl: true };
//! let mut keys = Translator::new(swap, Os::MacOs, Os::Windows);
//! let injected: Vec<_> = keys_typed
//!     .iter()
//!     .map(|&key| to_windows_scancode(keys.press(key)))
//!     .collect();
//! assert_eq!(injected, [Some((0x1D, false)), Some((0x2E, false))]);
//! ```
//!
//! # Keys that map to `None`
//!
//! Every lookup returns `None` for a key it does not know, and the caller
//! drops such keys. Every key of a US keyboard is known on both OSes, except
//! where a key exists on only one of them:
//! - No macOS keycode: Print Screen, Scroll Lock, Pause, F21 to F24, the
//!   Japanese Convert and NonConvert keys, and the media transport keys (Play/
//!   Pause, Stop, Next and Previous Track), which macOS reports as
//!   system-defined events rather than keycodes.
//! - Mac keys that are not forwarded: Fn (`kVK_Function`) and the JIS Eisu
//!   key (`kVK_JIS_Eisu`); [`from_mac`] returns `None` for them.
//!
//! These keys are left out on purpose and map to `None` everywhere:
//! - Power, Sleep and Wake, so a stray press cannot put the other computer to
//!   sleep or shut it down.
//! - The keyboard-page volume keys (HID 0x7F to 0x81). Keyboards send the
//!   consumer-page ones (Mute 0xE2, Volume Up 0xE9, Volume Down 0xEA), and
//!   those are known.
//! - The ISO `#~` key (HID 0x32): Windows gives it the same scancode as
//!   Backslash, so the two cannot be told apart.
//! - Legacy keys that neither laptop has and a Mac has no keycode for (Help,
//!   Undo, Cut, Copy, Paste, Find and the like), the browser and app-launch
//!   keys, and the Korean Lang keys, whose Windows scancodes differ between
//!   sources.
//! - Windows variants of known keys: Ctrl+Pause (Break, E0 46) and Alt+Print
//!   Screen (SysRq, 0x54), which a Mac has no key for anyway, and the "fake
//!   shift" codes (E0 2A, E0 36) some keyboards add around the navigation
//!   keys. Those are noise: real Shift presses arrive separately.
//!
//! [`KeyCode`]: hop_types::KeyCode

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod buttons;
mod policy;
mod table;
mod translator;

pub use buttons::ButtonTracker;
pub use policy::ModifierPolicy;
pub use table::{from_mac, from_name, from_windows_scancode, name, to_mac, to_windows_scancode};
pub use translator::Translator;
