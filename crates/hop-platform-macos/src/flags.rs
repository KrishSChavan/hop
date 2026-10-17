//! Modifier keys and the event flags that go with them.
//!
//! A Quartz event carries two kinds of modifier flags: the generic ones
//! (`kCGEventFlagMaskShift` and friends, set while either side's key is
//! down) and device-dependent bits that say which side (the `NX_DEVICE*`
//! masks in IOKit's `IOLLEvent.h`). Capture reads both to tell a left key
//! from a right one; injection writes both, the way a real keyboard does.

use objc2_core_graphics::CGEventFlags;

/// `kVK_CapsLock`.
pub(crate) const CAPS_LOCK: u16 = 0x39;

/// A modifier key: its macOS keycode, its generic flag and its
/// device-dependent bit.
#[derive(Clone, Copy, Debug)]
struct Modifier {
    keycode: u16,
    generic: CGEventFlags,
    device: u64,
}

const fn modifier(keycode: u16, generic: CGEventFlags, device: u64) -> Modifier {
    Modifier {
        keycode,
        generic,
        device,
    }
}

/// The eight modifiers, with their `kVK_*` keycodes and `NX_DEVICE*` bits.
/// Caps Lock and Fn are not here: Caps Lock is a toggle, and Fn is never
/// forwarded (see `hop-keymap`).
const MODIFIERS: [Modifier; 8] = [
    modifier(0x3B, CGEventFlags::MaskControl, 0x0000_0001), // kVK_Control, NX_DEVICELCTLKEYMASK
    modifier(0x38, CGEventFlags::MaskShift, 0x0000_0002),   // kVK_Shift, NX_DEVICELSHIFTKEYMASK
    modifier(0x3C, CGEventFlags::MaskShift, 0x0000_0004), // kVK_RightShift, NX_DEVICERSHIFTKEYMASK
    modifier(0x37, CGEventFlags::MaskCommand, 0x0000_0008), // kVK_Command, NX_DEVICELCMDKEYMASK
    modifier(0x36, CGEventFlags::MaskCommand, 0x0000_0010), // kVK_RightCommand, NX_DEVICERCMDKEYMASK
    modifier(0x3A, CGEventFlags::MaskAlternate, 0x0000_0020), // kVK_Option, NX_DEVICELALTKEYMASK
    modifier(0x3D, CGEventFlags::MaskAlternate, 0x0000_0040), // kVK_RightOption, NX_DEVICERALTKEYMASK
    modifier(0x3E, CGEventFlags::MaskControl, 0x0000_2000), // kVK_RightControl, NX_DEVICERCTLKEYMASK
];

/// Every device-dependent modifier bit.
const DEVICE_BITS: u64 = 0x0000_207F;

/// Which of the eight modifiers are down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModifierSet(u8);

/// The position of `keycode` in [`MODIFIERS`], if it is one of them.
fn index(keycode: u16) -> Option<usize> {
    MODIFIERS.iter().position(|m| m.keycode == keycode)
}

/// Whether `keycode` is one of the eight modifier keys.
pub(crate) fn is_modifier(keycode: u16) -> bool {
    index(keycode).is_some()
}

impl ModifierSet {
    /// Whether the modifier with this keycode is down. Other keys are never
    /// "down" here.
    pub(crate) fn contains(self, keycode: u16) -> bool {
        index(keycode).is_some_and(|i| self.0 & (1 << i) != 0)
    }

    /// Marks the modifier with this keycode as down or up; other keys are
    /// ignored.
    pub(crate) fn set(&mut self, keycode: u16, down: bool) {
        if let Some(i) = index(keycode) {
            if down {
                self.0 |= 1 << i;
            } else {
                self.0 &= !(1 << i);
            }
        }
    }

    /// The flags a real keyboard reports while exactly these modifiers are
    /// down: the generic flag of each, plus its side's device bit.
    pub(crate) fn flags(self) -> CGEventFlags {
        MODIFIERS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.0 & (1 << i) != 0)
            .fold(CGEventFlags(0), |flags, (_, m)| {
                CGEventFlags(flags.0 | m.generic.0 | m.device)
            })
    }

    /// Records a flags-changed event for modifier `keycode` and returns
    /// whether the key went down (`true`) or up (`false`), or `None` if
    /// `keycode` is not a modifier.
    ///
    /// The side's device bit decides when the event carries device bits,
    /// so releasing Left Shift while Right Shift stays down still reads as
    /// a release. Some synthetic events carry only the generic flags; then
    /// a clear flag means released, and a set flag flips what this set
    /// last recorded.
    pub(crate) fn record(&mut self, keycode: u16, flags: CGEventFlags) -> Option<bool> {
        let m = MODIFIERS[index(keycode)?];
        let down = if flags.0 & m.generic.0 == 0 {
            false
        } else if flags.0 & DEVICE_BITS != 0 {
            flags.0 & m.device != 0
        } else {
            !self.contains(keycode)
        };
        self.set(keycode, down);
        Some(down)
    }
}

/// Flags a real keyboard adds for a key regardless of the modifiers held.
///
/// AppKit documents that arrow-key events carry the numeric-pad and
/// function flags, keypad keys the numeric-pad flag, and the F keys and the
/// navigation keys (Help, Forward Delete, Home, End, Page Up, Page Down) the
/// function flag. Some apps look at these, so injected keys carry them too.
pub(crate) fn key_flags(keycode: u16) -> CGEventFlags {
    const NUMERIC_PAD: u64 = CGEventFlags::MaskNumericPad.0;
    const FUNCTION: u64 = CGEventFlags::MaskSecondaryFn.0;
    let bits = match keycode {
        // kVK_LeftArrow, kVK_RightArrow, kVK_DownArrow, kVK_UpArrow
        0x7B..=0x7E => NUMERIC_PAD | FUNCTION,
        // kVK_ANSI_KeypadDecimal, Multiply, Plus, Clear, Divide, Enter,
        // Minus, Equals, Keypad0 to Keypad7, Keypad8, Keypad9
        0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51..=0x59 | 0x5B | 0x5C => NUMERIC_PAD,
        // kVK_F1 to kVK_F20
        0x7A | 0x78 | 0x63 | 0x76 | 0x60 | 0x61 | 0x62 | 0x64 | 0x65 | 0x6D | 0x67 | 0x6F
        | 0x69 | 0x6B | 0x71 | 0x6A | 0x40 | 0x4F | 0x50 | 0x5A => FUNCTION,
        // kVK_Help, kVK_Home, kVK_PageUp, kVK_ForwardDelete, kVK_End,
        // kVK_PageDown
        0x72..=0x75 | 0x77 | 0x79 => FUNCTION,
        _ => 0,
    };
    CGEventFlags(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT_SHIFT: u16 = 0x38;
    const RIGHT_SHIFT: u16 = 0x3C;
    const LEFT_COMMAND: u16 = 0x37;
    const RIGHT_CONTROL: u16 = 0x3E;

    #[test]
    fn flags_name_the_generic_flag_and_the_side() {
        let mut set = ModifierSet::default();
        assert_eq!(set.flags(), CGEventFlags(0));
        set.set(LEFT_COMMAND, true);
        set.set(RIGHT_CONTROL, true);
        assert_eq!(
            set.flags(),
            CGEventFlags(CGEventFlags::MaskCommand.0 | 0x08 | CGEventFlags::MaskControl.0 | 0x2000)
        );
        set.set(LEFT_COMMAND, false);
        assert_eq!(
            set.flags(),
            CGEventFlags(CGEventFlags::MaskControl.0 | 0x2000)
        );
        set.set(0x00, true); // kVK_ANSI_A is not a modifier
        assert_eq!(
            set.flags(),
            CGEventFlags(CGEventFlags::MaskControl.0 | 0x2000)
        );
    }

    #[test]
    fn device_bits_tell_left_from_right() {
        let shift = CGEventFlags::MaskShift.0;
        let mut set = ModifierSet::default();
        // Left Shift down, then Right Shift down.
        assert_eq!(
            set.record(LEFT_SHIFT, CGEventFlags(shift | 0x02)),
            Some(true)
        );
        assert_eq!(
            set.record(RIGHT_SHIFT, CGEventFlags(shift | 0x06)),
            Some(true)
        );
        // Left Shift up while Right Shift stays down: the generic flag is
        // still set, but the left device bit is gone.
        assert_eq!(
            set.record(LEFT_SHIFT, CGEventFlags(shift | 0x04)),
            Some(false)
        );
        assert!(set.contains(RIGHT_SHIFT) && !set.contains(LEFT_SHIFT));
        assert_eq!(set.record(RIGHT_SHIFT, CGEventFlags(0)), Some(false));
        assert_eq!(set, ModifierSet::default());
    }

    #[test]
    fn generic_flags_alone_still_pair_downs_and_ups() {
        let command = CGEventFlags(CGEventFlags::MaskCommand.0);
        let mut set = ModifierSet::default();
        assert_eq!(set.record(LEFT_COMMAND, command), Some(true));
        // Without device bits, a set flag for a key that is down means the
        // other side kept it set: this key went up.
        assert_eq!(set.record(LEFT_COMMAND, command), Some(false));
        assert_eq!(set.record(LEFT_COMMAND, command), Some(true));
        assert_eq!(set.record(LEFT_COMMAND, CGEventFlags(0)), Some(false));
    }

    #[test]
    fn non_modifiers_are_not_recorded() {
        let mut set = ModifierSet::default();
        assert_eq!(set.record(CAPS_LOCK, CGEventFlags::MaskAlphaShift), None);
        assert_eq!(set.record(0x3F, CGEventFlags::MaskSecondaryFn), None); // kVK_Function
        assert_eq!(set, ModifierSet::default());
        assert!(!is_modifier(CAPS_LOCK) && is_modifier(LEFT_SHIFT));
    }

    #[test]
    fn navigation_and_keypad_keys_carry_their_extra_flags() {
        let both = CGEventFlags(CGEventFlags::MaskNumericPad.0 | CGEventFlags::MaskSecondaryFn.0);
        assert_eq!(key_flags(0x7E), both); // kVK_UpArrow
        assert_eq!(key_flags(0x52), CGEventFlags::MaskNumericPad); // kVK_ANSI_Keypad0
        assert_eq!(key_flags(0x7A), CGEventFlags::MaskSecondaryFn); // kVK_F1
        assert_eq!(key_flags(0x75), CGEventFlags::MaskSecondaryFn); // kVK_ForwardDelete
        assert_eq!(key_flags(0x00), CGEventFlags(0)); // kVK_ANSI_A
        assert_eq!(key_flags(0x24), CGEventFlags(0)); // kVK_Return
    }
}
