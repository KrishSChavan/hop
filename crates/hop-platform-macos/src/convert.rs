//! Turning Quartz events into Hop's [`InputEvent`]s.
//!
//! # Scroll signs
//!
//! macOS reports a scroll wheel turned forward (away from the user), which
//! scrolls toward the top of a document, as a positive axis-1 delta, and a
//! scroll toward the left of a document as a positive axis-2 delta. The
//! deltas already include the user's natural-scrolling preference: AppKit's
//! `NSEvent.h` says the deltas "are automatically inverted ... according to
//! the user's preferences", and `isDirectionInvertedFromDevice` exists to
//! undo that. Hop's convention (see `hop_types::InputEvent::Scroll`) is
//! positive `dy` for forward and positive `dx` for right, so
//! `dy = axis 1` and `dx = -axis 2`, and injection does the reverse.

use hop_types::{InputEvent, KeyCode, MouseButton, Point, ScrollUnit};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventMask, CGEventType};

use crate::flags::{self, ModifierSet};

/// The event types the tap listens to.
const TAPPED: [CGEventType; 14] = [
    CGEventType::MouseMoved,
    CGEventType::LeftMouseDragged,
    CGEventType::RightMouseDragged,
    CGEventType::OtherMouseDragged,
    CGEventType::LeftMouseDown,
    CGEventType::LeftMouseUp,
    CGEventType::RightMouseDown,
    CGEventType::RightMouseUp,
    CGEventType::OtherMouseDown,
    CGEventType::OtherMouseUp,
    CGEventType::ScrollWheel,
    CGEventType::KeyDown,
    CGEventType::KeyUp,
    CGEventType::FlagsChanged,
];

/// The tap's event mask (`CGEventMaskBit` of each tapped type).
pub(crate) fn tap_mask() -> CGEventMask {
    TAPPED.iter().fold(0, |mask, ty| mask | (1 << ty.0))
}

/// What one Quartz event becomes: up to two Hop events (a Caps Lock toggle
/// becomes a press and a release) and the cursor position before it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Converted {
    /// The cursor position after the previous event.
    pub(crate) cursor: Point,
    first: Option<InputEvent>,
    second: Option<InputEvent>,
}

impl Converted {
    /// The Hop events, in order.
    pub(crate) fn events(&self) -> impl Iterator<Item = InputEvent> + use<> {
        self.first.into_iter().chain(self.second)
    }
}

/// Converts the events of one tap, remembering what it needs between
/// events: the cursor position and which modifiers are down.
#[derive(Debug, Default)]
pub(crate) struct Converter {
    last_cursor: Option<Point>,
    modifiers: ModifierSet,
}

impl Converter {
    /// A converter that takes `cursor` as the position before the first
    /// event.
    pub(crate) fn new(cursor: Option<Point>) -> Self {
        Self {
            last_cursor: cursor,
            modifiers: ModifierSet::default(),
        }
    }

    /// Something other than an event moved the cursor (a warp).
    pub(crate) fn set_cursor(&mut self, cursor: Point) {
        self.last_cursor = Some(cursor);
    }

    /// Converts one event of type `ty`.
    ///
    /// Events Hop has no use for, keys `hop-keymap` does not know, and
    /// motion of zero length convert to nothing, but still update the
    /// tracked position and modifiers.
    pub(crate) fn convert(&mut self, ty: CGEventType, event: &CGEvent) -> Converted {
        let location = CGEvent::location(Some(event));
        let here = Point::new(location.x, location.y);
        let mut out = Converted {
            cursor: self.last_cursor.unwrap_or(here),
            first: None,
            second: None,
        };
        let mut mouse = true;
        match ty {
            CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged => out.first = motion(event),
            CGEventType::LeftMouseDown => out.first = button(MouseButton::Left, true),
            CGEventType::LeftMouseUp => out.first = button(MouseButton::Left, false),
            CGEventType::RightMouseDown => out.first = button(MouseButton::Right, true),
            CGEventType::RightMouseUp => out.first = button(MouseButton::Right, false),
            CGEventType::OtherMouseDown => out.first = other_button(event, true),
            CGEventType::OtherMouseUp => out.first = other_button(event, false),
            CGEventType::ScrollWheel => out.first = scroll(event),
            CGEventType::KeyDown | CGEventType::KeyUp => {
                mouse = false;
                out.first = key(event, ty == CGEventType::KeyDown);
            }
            CGEventType::FlagsChanged => {
                mouse = false;
                (out.first, out.second) = self.flags_changed(event);
            }
            _ => mouse = false,
        }
        // Keyboard events carry a location too, but only mouse events are
        // trusted to say where the cursor is.
        if mouse {
            self.last_cursor = Some(here);
        }
        out
    }

    /// A modifier went down or up, or Caps Lock toggled.
    fn flags_changed(&mut self, event: &CGEvent) -> (Option<InputEvent>, Option<InputEvent>) {
        let Some(keycode) = keycode(event) else {
            return (None, None);
        };
        let Some(code) = hop_keymap::from_mac(keycode) else {
            return (None, None);
        };
        if keycode == flags::CAPS_LOCK {
            // macOS reports Caps Lock once per press, as the lock state
            // flips, and never reports its release. One physical press is
            // one press and release on the other side.
            return (
                Some(key_event(code, true, false)),
                Some(key_event(code, false, false)),
            );
        }
        let pressed = self.modifiers.record(keycode, CGEvent::flags(Some(event)));
        (pressed.map(|pressed| key_event(code, pressed, false)), None)
    }
}

/// Pointer motion from the event's delta fields, which keep counting when
/// the cursor is stopped at a screen edge or parked.
fn motion(event: &CGEvent) -> Option<InputEvent> {
    let dx = CGEvent::double_value_field(Some(event), CGEventField::MouseEventDeltaX);
    let dy = CGEvent::double_value_field(Some(event), CGEventField::MouseEventDeltaY);
    (dx != 0.0 || dy != 0.0).then_some(InputEvent::PointerMotion { dx, dy })
}

fn button(button: MouseButton, pressed: bool) -> Option<InputEvent> {
    Some(InputEvent::Button { button, pressed })
}

/// Buttons beyond left and right: Quartz numbers them 2 (middle), 3 (back)
/// and 4 (forward). Higher-numbered buttons are dropped.
fn other_button(event: &CGEvent, pressed: bool) -> Option<InputEvent> {
    let number = CGEvent::integer_value_field(Some(event), CGEventField::MouseEventButtonNumber);
    let mouse_button = match number {
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => return None,
    };
    button(mouse_button, pressed)
}

/// Scrolling: smooth (trackpad, Magic Mouse) events in points, wheel
/// events in lines. See the module docs for the signs.
fn scroll(event: &CGEvent) -> Option<InputEvent> {
    let get_int = |field| CGEvent::integer_value_field(Some(event), field) as f64;
    let get_double = |field| CGEvent::double_value_field(Some(event), field);
    let continuous =
        CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventIsContinuous) != 0;
    let (vertical, horizontal, unit) = if continuous {
        (
            get_int(CGEventField::ScrollWheelEventPointDeltaAxis1),
            get_int(CGEventField::ScrollWheelEventPointDeltaAxis2),
            ScrollUnit::Pixel,
        )
    } else {
        // The fixed-point line deltas keep the fractions of a line that
        // accelerated wheels produce; the integer ones round them away.
        let lines = |fixed, whole| {
            let precise = get_double(fixed);
            if precise != 0.0 {
                precise
            } else {
                get_int(whole)
            }
        };
        (
            lines(
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
                CGEventField::ScrollWheelEventDeltaAxis1,
            ),
            lines(
                CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
                CGEventField::ScrollWheelEventDeltaAxis2,
            ),
            ScrollUnit::Line,
        )
    };
    if vertical == 0.0 && horizontal == 0.0 {
        return None;
    }
    Some(InputEvent::Scroll {
        // `0.0 - x` rather than `-x`, so no scrolling stays +0.0.
        dx: 0.0 - horizontal,
        dy: vertical,
        unit,
    })
}

/// A key-down or key-up, with auto-repeat as macOS reports it.
fn key(event: &CGEvent, pressed: bool) -> Option<InputEvent> {
    let code = hop_keymap::from_mac(keycode(event)?)?;
    let repeat = pressed
        && CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventAutorepeat) != 0;
    Some(key_event(code, pressed, repeat))
}

fn key_event(code: KeyCode, pressed: bool, repeat: bool) -> InputEvent {
    InputEvent::Key {
        code,
        pressed,
        repeat,
    }
}

/// The macOS virtual keycode of a keyboard event.
fn keycode(event: &CGEvent) -> Option<u16> {
    u16::try_from(CGEvent::integer_value_field(
        Some(event),
        CGEventField::KeyboardEventKeycode,
    ))
    .ok()
}

#[cfg(test)]
mod tests {
    //! These tests build Quartz events in memory and read them back. None is
    //! ever posted, so nothing reaches the screen or any app.

    use objc2_core_foundation::{CFRetained, CGPoint};
    use objc2_core_graphics::{CGEventFlags, CGMouseButton, CGScrollEventUnit};

    use super::*;

    fn mouse(ty: CGEventType, x: f64, y: f64, button: CGMouseButton) -> CFRetained<CGEvent> {
        CGEvent::new_mouse_event(None, ty, CGPoint::new(x, y), button).expect("mouse event")
    }

    fn moved(x: f64, y: f64, dx: f64, dy: f64) -> CFRetained<CGEvent> {
        let event = mouse(CGEventType::MouseMoved, x, y, CGMouseButton::Left);
        CGEvent::set_double_value_field(Some(&event), CGEventField::MouseEventDeltaX, dx);
        CGEvent::set_double_value_field(Some(&event), CGEventField::MouseEventDeltaY, dy);
        event
    }

    fn keyboard(keycode: u16, down: bool) -> CFRetained<CGEvent> {
        CGEvent::new_keyboard_event(None, keycode, down).expect("keyboard event")
    }

    fn convert(converter: &mut Converter, event: &CGEvent) -> (Point, Vec<InputEvent>) {
        let converted = converter.convert(CGEvent::r#type(Some(event)), event);
        (converted.cursor, converted.events().collect())
    }

    #[test]
    fn the_mask_covers_every_tapped_type() {
        let mask = tap_mask();
        assert_eq!(mask.count_ones(), 14);
        assert_ne!(mask & (1 << CGEventType::FlagsChanged.0), 0);
        assert_ne!(mask & (1 << CGEventType::OtherMouseDragged.0), 0);
        assert_eq!(mask & (1 << CGEventType::TabletPointer.0), 0);
    }

    #[test]
    fn motion_reports_the_attempted_delta_and_the_previous_position() {
        let mut converter = Converter::new(Some(Point::new(100.0, 100.0)));
        let (cursor, events) = convert(&mut converter, &moved(103.0, 99.0, 3.0, -1.0));
        assert_eq!(cursor, Point::new(100.0, 100.0));
        assert_eq!(events, [InputEvent::PointerMotion { dx: 3.0, dy: -1.0 }]);

        // Stopped at the right edge of a 1512-wide screen while still
        // pushing: the location stays put, the delta keeps coming, and the
        // position before the event lets the core see the push.
        let (_, _) = convert(&mut converter, &moved(1511.0, 300.0, 4.0, 0.0));
        let (cursor, events) = convert(&mut converter, &moved(1511.0, 300.0, 6.0, 0.0));
        assert_eq!(cursor, Point::new(1511.0, 300.0));
        assert_eq!(events, [InputEvent::PointerMotion { dx: 6.0, dy: 0.0 }]);
    }

    #[test]
    fn a_warp_resets_the_previous_position_and_zero_motion_is_dropped() {
        let mut converter = Converter::new(None);
        // With nothing known yet, the event's own location stands in.
        let (cursor, _) = convert(&mut converter, &moved(10.0, 20.0, 1.0, 1.0));
        assert_eq!(cursor, Point::new(10.0, 20.0));

        converter.set_cursor(Point::new(756.0, 491.0));
        let (cursor, events) = convert(&mut converter, &moved(756.0, 491.0, 0.0, 0.0));
        assert_eq!(cursor, Point::new(756.0, 491.0));
        assert!(events.is_empty());
    }

    #[test]
    fn drags_are_motion() {
        let mut converter = Converter::new(None);
        let event = mouse(
            CGEventType::LeftMouseDragged,
            50.0,
            50.0,
            CGMouseButton::Left,
        );
        CGEvent::set_double_value_field(Some(&event), CGEventField::MouseEventDeltaX, -2.0);
        let (_, events) = convert(&mut converter, &event);
        assert_eq!(events, [InputEvent::PointerMotion { dx: -2.0, dy: 0.0 }]);
    }

    #[test]
    fn buttons_map_left_right_middle_back_and_forward() {
        let mut converter = Converter::new(None);
        let cases = [
            (CGEventType::LeftMouseDown, 0, MouseButton::Left, true),
            (CGEventType::LeftMouseUp, 0, MouseButton::Left, false),
            (CGEventType::RightMouseDown, 1, MouseButton::Right, true),
            (CGEventType::RightMouseUp, 1, MouseButton::Right, false),
            (CGEventType::OtherMouseDown, 2, MouseButton::Middle, true),
            (CGEventType::OtherMouseUp, 3, MouseButton::Back, false),
            (CGEventType::OtherMouseDown, 4, MouseButton::Forward, true),
        ];
        for (ty, number, button, pressed) in cases {
            let event = mouse(ty, 5.0, 5.0, CGMouseButton(number));
            let (_, events) = convert(&mut converter, &event);
            assert_eq!(
                events,
                [InputEvent::Button { button, pressed }],
                "{ty:?} {number}"
            );
        }
        // A sixth button has no Hop name.
        let event = mouse(CGEventType::OtherMouseDown, 5.0, 5.0, CGMouseButton(5));
        assert!(convert(&mut converter, &event).1.is_empty());
    }

    #[test]
    fn wheel_scrolling_is_in_lines_with_hop_signs() {
        let mut converter = Converter::new(None);
        // Wheel forward one notch, and a notch of "scroll left".
        let event = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 2, 1, 2, 0)
            .expect("scroll");
        let (_, events) = convert(&mut converter, &event);
        assert_eq!(
            events,
            [InputEvent::Scroll {
                dx: -2.0,
                dy: 1.0,
                unit: ScrollUnit::Line
            }]
        );
    }

    #[test]
    fn smooth_scrolling_is_in_points() {
        let mut converter = Converter::new(None);
        let event = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, -7, 3, 0)
            .expect("scroll");
        let (_, events) = convert(&mut converter, &event);
        assert_eq!(
            events,
            [InputEvent::Scroll {
                dx: -3.0,
                dy: -7.0,
                unit: ScrollUnit::Pixel
            }]
        );
    }

    #[test]
    fn keys_convert_through_the_keymap_with_auto_repeat() {
        let mut converter = Converter::new(None);
        let a = KeyCode::keyboard(0x04);
        let (_, events) = convert(&mut converter, &keyboard(0x00, true)); // kVK_ANSI_A
        assert_eq!(events, [key_event(a, true, false)]);

        let repeat = keyboard(0x00, true);
        CGEvent::set_integer_value_field(Some(&repeat), CGEventField::KeyboardEventAutorepeat, 1);
        assert_eq!(
            convert(&mut converter, &repeat).1,
            [key_event(a, true, true)]
        );
        assert_eq!(
            convert(&mut converter, &keyboard(0x00, false)).1,
            [key_event(a, false, false)]
        );

        // kVK_Function is never forwarded.
        let function = keyboard(0x3F, true);
        assert!(convert(&mut converter, &function).1.is_empty());
    }

    #[test]
    fn keyboard_events_do_not_move_the_tracked_cursor() {
        let mut converter = Converter::new(Some(Point::new(1.0, 1.0)));
        let event = keyboard(0x00, true);
        CGEvent::set_location(Some(&event), CGPoint::new(500.0, 500.0));
        assert_eq!(convert(&mut converter, &event).0, Point::new(1.0, 1.0));
        assert_eq!(convert(&mut converter, &event).0, Point::new(1.0, 1.0));
    }

    #[test]
    fn modifier_presses_and_releases_come_from_the_flags() {
        let mut converter = Converter::new(None);
        // kVK_Command: the keyboard API makes this a flags-changed event.
        let down = keyboard(0x37, true);
        assert_eq!(CGEvent::r#type(Some(&down)), CGEventType::FlagsChanged);
        CGEvent::set_flags(
            Some(&down),
            CGEventFlags(CGEventFlags::MaskCommand.0 | 0x08),
        );
        let (_, events) = convert(&mut converter, &down);
        assert_eq!(events, [key_event(KeyCode::LEFT_META, true, false)]);

        let up = keyboard(0x37, false);
        CGEvent::set_flags(Some(&up), CGEventFlags(0));
        let (_, events) = convert(&mut converter, &up);
        assert_eq!(events, [key_event(KeyCode::LEFT_META, false, false)]);
    }

    #[test]
    fn caps_lock_becomes_a_press_and_a_release() {
        let mut converter = Converter::new(None);
        let toggle = keyboard(flags::CAPS_LOCK, true);
        CGEvent::set_type(Some(&toggle), CGEventType::FlagsChanged);
        CGEvent::set_flags(Some(&toggle), CGEventFlags::MaskAlphaShift);
        let (_, events) = convert(&mut converter, &toggle);
        assert_eq!(
            events,
            [
                key_event(KeyCode::CAPS_LOCK, true, false),
                key_event(KeyCode::CAPS_LOCK, false, false)
            ]
        );
    }
}
