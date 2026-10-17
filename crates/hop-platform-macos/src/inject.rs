//! Posting input into macOS: moves, clicks, keys and scrolling.
//!
//! Every event comes from one private event source, carries
//! [`INJECTED_TAG`] in `kCGEventSourceUserData`, and is posted at the HID
//! level, where it looks like hardware input to every app (and to Hop's own
//! tap, which reports it with `physical: false`).
//!
//! # Mouse event numbers
//!
//! Every mouse-down gets a new `kCGMouseEventNumber`, its mouse-up reuses
//! it, and drags reuse the number of the button they drag with, the way a
//! real mouse numbers its events; macOS 27 needs this to match clicks up.
//! The field is 16 bits wide (`SInt16 eventNum` in IOKit's `IOLLEvent.h`),
//! so Hop counts upward through the top half of the positive range,
//! 0x4000 to 0x7FFF, well above where a real mouse's counter is for its
//! first sixteen thousand clicks after boot. The starting point comes from
//! the system uptime, so a restarted Hop does not reuse the numbers it
//! used just before.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use hop_platform::{INJECTED_TAG, InputInjector, Permission, PlatformError, Result};
use hop_types::{InputEvent, KeyCode, MouseButton, Point, ScrollUnit};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSEvent;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFilterMask, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventSuppressionState, CGEventTapLocation, CGEventType, CGMouseButton,
    CGPreflightPostEventAccess, CGScrollEventUnit,
};
use objc2_foundation::NSProcessInfo;

use crate::cursor::current_position;
use crate::flags::{self, ModifierSet};
use crate::permissions::accessibility_trusted;

/// How far apart two presses may be, in points, and still count as one
/// double click. macOS does not publish its own value; a few points
/// forgives a shaky hand without merging separate clicks.
const CLICK_SLOP: f64 = 4.0;

/// The double-click interval if macOS does not say.
const DEFAULT_DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// The largest scroll a single injected event carries. Wheel deltas are
/// 16-bit fields; anything near that is a bug upstream, not a real scroll.
const MAX_SCROLL_LINES: f64 = 100.0;
const MAX_SCROLL_POINTS: f64 = 10_000.0;

/// [`InputInjector`] for macOS.
pub(crate) struct MacInjector {
    source: EventSource,
    /// Keys this injector holds down, by macOS keycode.
    keys: BTreeSet<u16>,
    /// The modifiers among `keys`, which set the flags of every event.
    modifiers: ModifierSet,
    /// Buttons this injector holds down. The order (left, right, middle,
    /// back, forward) also picks the button a drag is reported with.
    buttons: BTreeMap<MouseButton, Press>,
    clicks: ClickCounter,
    numbers: EventNumbers,
    /// Where this injector last put the cursor, so clicks land there even
    /// before macOS has caught up with the move.
    position: Option<Point>,
    /// Scrolling not yet sent, in fractions of a line or a point.
    lines: Remainder,
    points: Remainder,
    /// Posting has been checked to be allowed.
    allowed: bool,
    /// Asks whether posting is allowed: [`posting_permitted`], except in
    /// tests.
    gate: Gate,
    /// Where events go: [`post_to_macos`], except in tests.
    poster: Poster,
}

/// Delivers a finished event.
type Poster = fn(&CGEvent);

/// Says whether macOS lets Hop post events.
type Gate = fn() -> bool;

/// Posts at the HID level, where events enter the system as if from
/// hardware. Nothing else in this crate posts events.
fn post_to_macos(event: &CGEvent) {
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
}

/// Whether macOS lets Hop post events, asked without showing a prompt.
fn posting_permitted() -> bool {
    posting_granted(accessibility_trusted(false), || {
        CGPreflightPostEventAccess()
    })
}

/// Either of two grants lets a process post events: Accessibility, which
/// is what Hop asks for, or event posting on its own, which macOS grants
/// when an app posts before asking. Apple documents
/// `CGPreflightPostEventAccess` as checking only the second, so on its own
/// it could say no after the user grants Accessibility through Hop's
/// prompt, and every injection would fail. Neither grant means nothing is
/// posted, so posting can never be what triggers a grant.
fn posting_granted(accessibility: bool, post_event_access: impl FnOnce() -> bool) -> bool {
    accessibility || post_event_access()
}

/// The private event source.
struct EventSource(CFRetained<CGEventSource>);

// SAFETY: a CGEventSource is a CoreFoundation object, which may move between
// threads as long as it is used by one thread at a time. It is only reached
// through `&mut MacInjector`, and after creation it is only read (to create
// events from it).
unsafe impl Send for EventSource {}

/// A held button's event number and click count, reused by its up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Press {
    number: i64,
    clicks: i64,
}

impl MacInjector {
    /// Creates the private event source. Nothing is posted until the first
    /// event is injected.
    pub(crate) fn new() -> Result<Self> {
        Self::with_poster(posting_permitted, post_to_macos)
    }

    /// An injector that asks `gate` before it first posts and sends its
    /// finished events to `poster`.
    fn with_poster(gate: Gate, poster: Poster) -> Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .ok_or_else(|| PlatformError::Os("could not create an event source".into()))?;
        // Events from this source carry the tag even before `post` sets it.
        CGEventSource::set_user_data(Some(&source), INJECTED_TAG);
        // By default macOS ignores local hardware input for a quarter second
        // after each posted event. Hop must keep seeing the local mouse and
        // keyboard while it injects, or the user could not take control back.
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        let all = CGEventFilterMask::PermitLocalMouseEvents
            | CGEventFilterMask::PermitLocalKeyboardEvents
            | CGEventFilterMask::PermitSystemDefinedEvents;
        for state in [
            CGEventSuppressionState::EventSuppressionStateSuppressionInterval,
            CGEventSuppressionState::EventSuppressionStateRemoteMouseDrag,
        ] {
            CGEventSource::set_local_events_filter_during_suppression_state(
                Some(&source),
                all,
                state,
            );
        }
        // This runs on whatever thread builds the platform, which may have
        // no autorelease pool; Foundation may autorelease objects here.
        let uptime = autoreleasepool(|_| NSProcessInfo::processInfo().systemUptime());
        Ok(Self {
            source: EventSource(source),
            keys: BTreeSet::new(),
            modifiers: ModifierSet::default(),
            buttons: BTreeMap::new(),
            clicks: ClickCounter::default(),
            numbers: EventNumbers::seeded(uptime),
            position: None,
            lines: Remainder::default(),
            points: Remainder::default(),
            allowed: false,
            gate,
            poster,
        })
    }

    /// Refuses to post until macOS allows it. Posting without the
    /// Accessibility permission does nothing, and doing it before Hop asks
    /// for the permission can leave Hop with a posting-only grant.
    fn check_allowed(&mut self) -> Result<()> {
        if !self.allowed {
            if !(self.gate)() {
                return Err(PlatformError::PermissionDenied(Permission::Accessibility));
            }
            self.allowed = true;
        }
        Ok(())
    }

    /// Where the next mouse event happens: where Hop last moved the cursor,
    /// else where the cursor is.
    fn here(&self) -> Result<Point> {
        match self.position {
            Some(position) => Ok(position),
            None => current_position(),
        }
    }

    /// Tags and posts `event` with the flags of the modifiers held now.
    fn post(&self, event: &CGEvent, extra_flags: CGEventFlags) {
        // Caps Lock stays whatever the Mac's own keyboard set it to.
        let caps_lock = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState).0
            & CGEventFlags::MaskAlphaShift.0
            != 0;
        CGEvent::set_flags(
            Some(event),
            event_flags(
                CGEvent::flags(Some(event)),
                self.modifiers,
                extra_flags,
                caps_lock,
            ),
        );
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::EventSourceUserData,
            INJECTED_TAG,
        );
        (self.poster)(event);
    }

    fn mouse_event(
        &self,
        ty: CGEventType,
        at: Point,
        button: MouseButton,
    ) -> Result<CFRetained<CGEvent>> {
        CGEvent::new_mouse_event(
            Some(&self.source.0),
            ty,
            CGPoint::new(at.x, at.y),
            CGMouseButton(button_number(button) as u32),
        )
        .ok_or_else(|| PlatformError::Os("could not create a mouse event".into()))
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        self.check_allowed()?;
        let at = self.here()?;
        let press = if pressed {
            let clicks = self
                .clicks
                .press(button, at, Instant::now(), double_click_interval());
            Press {
                number: self.numbers.take(),
                clicks,
            }
        } else {
            // An up reuses its down's number and count. An up for a button
            // this injector never pressed still gets a number of its own.
            match self.buttons.get(&button) {
                Some(press) => *press,
                None => Press {
                    number: self.numbers.take(),
                    clicks: 1,
                },
            }
        };
        let event = self.mouse_event(button_type(button, pressed), at, button)?;
        let set = |field, value| CGEvent::set_integer_value_field(Some(&event), field, value);
        set(CGEventField::MouseEventNumber, press.number);
        set(CGEventField::MouseEventClickState, press.clicks);
        set(CGEventField::MouseEventButtonNumber, button_number(button));
        CGEvent::set_double_value_field(
            Some(&event),
            CGEventField::MouseEventPressure,
            if pressed { 1.0 } else { 0.0 },
        );
        self.post(&event, CGEventFlags(0));
        if pressed {
            self.buttons.insert(button, press);
        } else {
            self.buttons.remove(&button);
        }
        self.position = Some(at);
        Ok(())
    }

    fn key(&mut self, code: KeyCode, pressed: bool, repeat: bool) -> Result<()> {
        // Keys with no macOS keycode are skipped, as the contract says.
        let Some(keycode) = hop_keymap::to_mac(code) else {
            return Ok(());
        };
        self.check_allowed()?;
        // The keyboard API makes modifier keys flags-changed events itself.
        let event = CGEvent::new_keyboard_event(Some(&self.source.0), keycode, pressed)
            .ok_or_else(|| PlatformError::Os("could not create a keyboard event".into()))?;
        if pressed && repeat {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::KeyboardEventAutorepeat,
                1,
            );
        }
        // A modifier's own event carries its new state, as a keyboard's does.
        self.modifiers.set(keycode, pressed);
        self.post(&event, flags::key_flags(keycode));
        if pressed {
            self.keys.insert(keycode);
        } else {
            self.keys.remove(&keycode);
        }
        Ok(())
    }

    fn scroll(&mut self, dx: f64, dy: f64, unit: ScrollUnit) -> Result<()> {
        self.check_allowed()?;
        let (units, limit, remainder) = match unit {
            ScrollUnit::Line => (CGScrollEventUnit::Line, MAX_SCROLL_LINES, &mut self.lines),
            ScrollUnit::Pixel => (
                CGScrollEventUnit::Pixel,
                MAX_SCROLL_POINTS,
                &mut self.points,
            ),
        };
        // Hop: +dy forward, +dx right. Quartz: +axis 1 forward, +axis 2
        // left (see the `convert` module).
        let (vertical, horizontal) = remainder.take(dy, 0.0 - dx, limit);
        if vertical == 0 && horizontal == 0 {
            return Ok(());
        }
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&self.source.0),
            units,
            2,
            vertical,
            horizontal,
            0,
        )
        .ok_or_else(|| PlatformError::Os("could not create a scroll event".into()))?;
        // Scroll where Hop last put the cursor, even if macOS has not caught
        // up with that move yet.
        if let Some(at) = self.position {
            CGEvent::set_location(Some(&event), CGPoint::new(at.x, at.y));
        }
        self.post(&event, CGEventFlags(0));
        Ok(())
    }
}

impl InputInjector for MacInjector {
    fn move_cursor_to(&mut self, to: Point) -> Result<()> {
        self.check_allowed()?;
        let from = self.here()?;
        // While a button is held this is a drag, reported with that
        // button's type and event number.
        let held = self
            .buttons
            .iter()
            .next()
            .map(|(button, press)| (*button, *press));
        let (ty, button) = match held {
            Some((button, _)) => (drag_type(button), button),
            None => (CGEventType::MouseMoved, MouseButton::Left),
        };
        let event = self.mouse_event(ty, to, button)?;
        // The delta fields hold whole points: report how far the cursor's
        // point moved, so the deltas of many small moves add up exactly.
        let (dx, dy) = whole_point_delta(from, to);
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaX, dx);
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaY, dy);
        if let Some((button, press)) = held {
            let set = |field, value| CGEvent::set_integer_value_field(Some(&event), field, value);
            set(CGEventField::MouseEventNumber, press.number);
            set(CGEventField::MouseEventButtonNumber, button_number(button));
            // A drag carries its press's click count, as a real mouse's
            // does (Quartz would say 1): after a double click it is 2, which
            // apps read as dragging out a selection word by word.
            set(CGEventField::MouseEventClickState, press.clicks);
        }
        self.post(&event, CGEventFlags(0));
        self.position = Some(to);
        Ok(())
    }

    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match *event {
            // Motion normally arrives through `move_cursor_to`; relative
            // motion is applied from the last position Hop set.
            InputEvent::PointerMotion { dx, dy } => {
                let from = self.here()?;
                self.move_cursor_to(Point::new(from.x + dx, from.y + dy))
            }
            InputEvent::Button { button, pressed } => self.button(button, pressed),
            InputEvent::Scroll { dx, dy, unit } => self.scroll(dx, dy, unit),
            InputEvent::Key {
                code,
                pressed,
                repeat,
            } => self.key(code, pressed, repeat),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let mut result = Ok(());
        let mut keep_first_error = |outcome: Result<()>| {
            if result.is_ok() {
                result = outcome;
            }
        };
        // Buttons first, so a drag ends where it is; then ordinary keys;
        // then modifiers, so the keys above still carried them.
        let buttons: Vec<MouseButton> = self.buttons.keys().copied().collect();
        for button in buttons {
            keep_first_error(self.button(button, false));
        }
        let (modifiers, keys): (Vec<u16>, Vec<u16>) = self
            .keys
            .iter()
            .copied()
            .partition(|&keycode| flags::is_modifier(keycode));
        for keycode in keys.into_iter().chain(modifiers) {
            keep_first_error(self.release_key(keycode));
        }
        // Whatever failed, nothing is tracked as held any more: a stale
        // record would only send a stray key-up next session.
        self.buttons.clear();
        self.keys.clear();
        self.modifiers = ModifierSet::default();
        // The next session starts from wherever the cursor really is.
        self.position = None;
        self.lines = Remainder::default();
        self.points = Remainder::default();
        result
    }
}

impl MacInjector {
    /// Posts the key-up for a held macOS keycode.
    fn release_key(&mut self, keycode: u16) -> Result<()> {
        self.check_allowed()?;
        let event = CGEvent::new_keyboard_event(Some(&self.source.0), keycode, false)
            .ok_or_else(|| PlatformError::Os("could not create a keyboard event".into()))?;
        self.modifiers.set(keycode, false);
        self.keys.remove(&keycode);
        self.post(&event, flags::key_flags(keycode));
        Ok(())
    }
}

impl Drop for MacInjector {
    fn drop(&mut self) {
        // Fail open: nothing Hop pressed stays pressed.
        let _ = self.release_all();
    }
}

/// The flags for an injected event: those the event was created with,
/// minus every modifier flag, plus the modifiers this injector holds, the
/// key's own extra flags, and Caps Lock if it is on.
fn event_flags(
    created: CGEventFlags,
    modifiers: ModifierSet,
    extra: CGEventFlags,
    caps_lock: bool,
) -> CGEventFlags {
    // Generic modifier flags, Caps Lock, and every device-dependent bit.
    const MODIFIER_BITS: u64 = CGEventFlags::MaskAlphaShift.0
        | CGEventFlags::MaskShift.0
        | CGEventFlags::MaskControl.0
        | CGEventFlags::MaskAlternate.0
        | CGEventFlags::MaskCommand.0
        | 0x0000_20FF;
    let caps = if caps_lock {
        CGEventFlags::MaskAlphaShift.0
    } else {
        0
    };
    CGEventFlags((created.0 & !MODIFIER_BITS) | modifiers.flags().0 | extra.0 | caps)
}

/// How many whole points the cursor moves from `from` to `to`: the change
/// in the nearest whole point, which is what the delta fields can hold.
fn whole_point_delta(from: Point, to: Point) -> (i64, i64) {
    let axis = |a: f64, b: f64| {
        let delta = b.round() - a.round();
        if delta.is_finite() {
            delta.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i64
        } else {
            0
        }
    };
    (axis(from.x, to.x), axis(from.y, to.y))
}

/// The double-click interval the user set in System Settings.
fn double_click_interval() -> Duration {
    // Injection runs on the runtime's own thread, which has no autorelease
    // pool. AppKit reads the setting through the user defaults, which may
    // autorelease objects; without a pool they would pile up, one batch per
    // click, for as long as that thread lives.
    let seconds = autoreleasepool(|_| NSEvent::doubleClickInterval());
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|interval| !interval.is_zero() && *interval < Duration::from_secs(5))
        .unwrap_or(DEFAULT_DOUBLE_CLICK)
}

/// Quartz's number for a button: 0 left, 1 right, 2 middle, 3 back,
/// 4 forward.
fn button_number(button: MouseButton) -> i64 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

fn button_type(button: MouseButton, pressed: bool) -> CGEventType {
    match (button, pressed) {
        (MouseButton::Left, true) => CGEventType::LeftMouseDown,
        (MouseButton::Left, false) => CGEventType::LeftMouseUp,
        (MouseButton::Right, true) => CGEventType::RightMouseDown,
        (MouseButton::Right, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    }
}

fn drag_type(button: MouseButton) -> CGEventType {
    match button {
        MouseButton::Left => CGEventType::LeftMouseDragged,
        MouseButton::Right => CGEventType::RightMouseDragged,
        _ => CGEventType::OtherMouseDragged,
    }
}

/// Counts clicks the way macOS does: a press of the same button soon
/// enough after the last one, and close enough to it, continues a double
/// or triple click.
#[derive(Debug, Default)]
struct ClickCounter {
    last: Option<LastPress>,
}

#[derive(Clone, Copy, Debug)]
struct LastPress {
    button: MouseButton,
    at: Point,
    time: Instant,
    clicks: i64,
}

impl ClickCounter {
    /// The click count for a press of `button` at `at`: 1 for a single
    /// click, 2 for the second press of a double click, and so on.
    fn press(&mut self, button: MouseButton, at: Point, now: Instant, interval: Duration) -> i64 {
        let clicks = match self.last {
            Some(last)
                if last.button == button
                    && now.saturating_duration_since(last.time) <= interval
                    && (at.x - last.at.x).hypot(at.y - last.at.y) <= CLICK_SLOP =>
            {
                last.clicks + 1
            }
            _ => 1,
        };
        self.last = Some(LastPress {
            button,
            at,
            time: now,
            clicks,
        });
        clicks
    }
}

/// Hands out mouse event numbers; see the module docs.
#[derive(Debug)]
struct EventNumbers {
    next: i64,
}

impl EventNumbers {
    const FIRST: i64 = 0x4000;
    const LAST: i64 = 0x7FFF;

    /// Starts at a point in the range picked by the system uptime in
    /// seconds, so successive runs of Hop start further along.
    fn seeded(uptime_seconds: f64) -> Self {
        let span = Self::LAST - Self::FIRST + 1;
        let offset = if uptime_seconds.is_finite() && uptime_seconds > 0.0 {
            (uptime_seconds as i64) % span
        } else {
            0
        };
        Self {
            next: Self::FIRST + offset,
        }
    }

    /// The next number, wrapping from 0x7FFF back to 0x4000.
    fn take(&mut self) -> i64 {
        let number = self.next;
        self.next = if number >= Self::LAST {
            Self::FIRST
        } else {
            number + 1
        };
        number
    }
}

/// Fractions not yet sent, for fields that only hold whole numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Remainder {
    a: f64,
    b: f64,
}

impl Remainder {
    /// Adds `(a, b)` and returns the whole part of each running total,
    /// keeping the fractions for next time. A total beyond `limit` is cut
    /// to it and its excess dropped, rather than paid out over later events.
    fn take(&mut self, a: f64, b: f64, limit: f64) -> (i32, i32) {
        fn whole(total: &mut f64, delta: f64, limit: f64) -> i32 {
            if !delta.is_finite() {
                return 0;
            }
            *total += delta;
            let part = total.trunc();
            *total -= part;
            part.clamp(-limit, limit) as i32
        }
        (whole(&mut self.a, a, limit), whole(&mut self.b, b, limit))
    }
}

#[cfg(test)]
mod tests {
    //! Nothing here posts an event. The injector tests swap the poster for
    //! a recorder that keeps a copy of each finished event, so the events
    //! are built in memory, inspected, and freed.

    use std::cell::RefCell;

    use super::*;
    use crate::convert::Converter;

    thread_local! {
        static POSTED: RefCell<Vec<CFRetained<CGEvent>>> = const { RefCell::new(Vec::new()) };
    }

    fn record(event: &CGEvent) {
        let copy = CGEvent::new_copy(Some(event)).expect("event copy");
        POSTED.with(|posted| posted.borrow_mut().push(copy));
    }

    /// The events "posted" since the last call.
    fn posted() -> Vec<CFRetained<CGEvent>> {
        POSTED.with(|posted| std::mem::take(&mut *posted.borrow_mut()))
    }

    /// An injector that records instead of posting, and does not ask macOS
    /// whether posting is allowed.
    fn recording_injector() -> MacInjector {
        MacInjector::with_poster(|| true, record).expect("injector")
    }

    fn int(event: &CGEvent, field: CGEventField) -> i64 {
        CGEvent::integer_value_field(Some(event), field)
    }

    fn ty(event: &CGEvent) -> CGEventType {
        CGEvent::r#type(Some(event))
    }

    /// The flags without Caps Lock, which follows the Mac's real state.
    fn flags(event: &CGEvent) -> u64 {
        CGEvent::flags(Some(event)).0 & !CGEventFlags::MaskAlphaShift.0
    }

    fn click(button: MouseButton, pressed: bool) -> InputEvent {
        InputEvent::Button { button, pressed }
    }

    fn key(usage: u16, pressed: bool) -> InputEvent {
        InputEvent::Key {
            code: KeyCode::keyboard(usage),
            pressed,
            repeat: false,
        }
    }

    #[test]
    fn a_double_click_numbers_and_counts_its_events() {
        let mut injector = recording_injector();
        injector.move_cursor_to(Point::new(300.0, 200.0)).unwrap();
        for pressed in [true, false, true, false] {
            injector.inject(&click(MouseButton::Left, pressed)).unwrap();
        }
        let events = posted();
        assert_eq!(events.len(), 5);
        let clicks = &events[1..];
        let types: Vec<_> = clicks.iter().map(|e| ty(e)).collect();
        use CGEventType as T;
        assert_eq!(
            types,
            [
                T::LeftMouseDown,
                T::LeftMouseUp,
                T::LeftMouseDown,
                T::LeftMouseUp
            ]
        );
        let numbers: Vec<_> = clicks
            .iter()
            .map(|e| int(e, CGEventField::MouseEventNumber))
            .collect();
        assert_eq!(numbers[0], numbers[1], "an up reuses its down's number");
        assert_eq!(numbers[2], numbers[3]);
        assert_eq!(numbers[2], numbers[0] + 1, "each down gets the next number");
        assert!((0x4000..=0x7FFF).contains(&numbers[0]));
        let counts: Vec<_> = clicks
            .iter()
            .map(|e| int(e, CGEventField::MouseEventClickState))
            .collect();
        assert_eq!(counts, [1, 1, 2, 2]);
        for event in &events {
            assert_eq!(int(event, CGEventField::EventSourceUserData), INJECTED_TAG);
            let at = CGEvent::location(Some(event));
            assert_eq!((at.x, at.y), (300.0, 200.0));
        }
    }

    #[test]
    fn moving_with_a_button_held_is_a_drag_with_that_buttons_number() {
        let mut injector = recording_injector();
        injector.move_cursor_to(Point::new(100.0, 100.0)).unwrap();
        injector.inject(&click(MouseButton::Right, true)).unwrap();
        injector.move_cursor_to(Point::new(103.0, 98.0)).unwrap();
        injector.inject(&click(MouseButton::Right, false)).unwrap();
        injector.move_cursor_to(Point::new(104.0, 98.0)).unwrap();
        let events = posted();
        let down = &events[1];
        let drag = &events[2];
        assert_eq!(ty(drag), CGEventType::RightMouseDragged);
        assert_eq!(
            int(drag, CGEventField::MouseEventNumber),
            int(down, CGEventField::MouseEventNumber)
        );
        assert_eq!(int(drag, CGEventField::MouseEventDeltaX), 3);
        assert_eq!(int(drag, CGEventField::MouseEventDeltaY), -2);
        assert_eq!(
            ty(&events[4]),
            CGEventType::MouseMoved,
            "released: a plain move again"
        );
    }

    #[test]
    fn a_drag_after_a_double_click_carries_its_click_count() {
        let mut injector = recording_injector();
        injector.move_cursor_to(Point::new(200.0, 200.0)).unwrap();
        for pressed in [true, false, true] {
            injector.inject(&click(MouseButton::Left, pressed)).unwrap();
        }
        injector.move_cursor_to(Point::new(240.0, 200.0)).unwrap();
        injector.inject(&click(MouseButton::Left, false)).unwrap();
        let events = posted();
        let second_down = &events[3];
        let drag = &events[4];
        assert_eq!(ty(drag), CGEventType::LeftMouseDragged);
        assert_eq!(int(second_down, CGEventField::MouseEventClickState), 2);
        assert_eq!(
            int(drag, CGEventField::MouseEventClickState),
            2,
            "the drag keeps the double click's count"
        );
        assert_eq!(
            int(drag, CGEventField::MouseEventNumber),
            int(second_down, CGEventField::MouseEventNumber)
        );
        assert_eq!(int(&events[5], CGEventField::MouseEventClickState), 2);
    }

    #[test]
    fn an_injector_without_permission_posts_nothing() {
        let mut injector = MacInjector::with_poster(|| false, record).expect("injector");
        let denied = |outcome: Result<()>| {
            assert!(
                matches!(
                    outcome,
                    Err(PlatformError::PermissionDenied(Permission::Accessibility))
                ),
                "{outcome:?}"
            );
        };
        denied(injector.move_cursor_to(Point::new(10.0, 10.0)));
        denied(injector.inject(&click(MouseButton::Left, true)));
        denied(injector.inject(&key(0x04, true)));
        denied(injector.inject(&InputEvent::Scroll {
            dx: 0.0,
            dy: 1.0,
            unit: ScrollUnit::Line,
        }));
        // Nothing was pressed, so there is nothing to release, on request
        // or on drop.
        assert!(injector.release_all().is_ok());
        drop(injector);
        assert!(posted().is_empty());
    }

    #[test]
    fn an_accessibility_grant_alone_allows_posting() {
        // The posting-only check is not even asked when Accessibility is
        // granted.
        assert!(posting_granted(true, || unreachable!()));
        assert!(posting_granted(false, || true));
        assert!(!posting_granted(false, || false));
    }

    #[test]
    fn other_buttons_carry_their_quartz_numbers() {
        let mut injector = recording_injector();
        injector.move_cursor_to(Point::new(10.0, 10.0)).unwrap();
        injector.inject(&click(MouseButton::Back, true)).unwrap();
        injector.inject(&click(MouseButton::Back, false)).unwrap();
        let events = posted();
        assert_eq!(ty(&events[1]), CGEventType::OtherMouseDown);
        assert_eq!(int(&events[1], CGEventField::MouseEventButtonNumber), 3);
        assert_eq!(ty(&events[2]), CGEventType::OtherMouseUp);
        assert_eq!(int(&events[2], CGEventField::MouseEventButtonNumber), 3);
    }

    #[test]
    fn keys_carry_the_modifiers_held_by_the_injector() {
        let mut injector = recording_injector();
        let command = CGEventFlags::MaskCommand.0 | 0x08;
        injector.inject(&key(0xE3, true)).unwrap(); // Left Meta: kVK_Command
        injector.inject(&key(0x06, true)).unwrap(); // C: kVK_ANSI_C
        injector.inject(&key(0x06, false)).unwrap();
        injector.inject(&key(0xE3, false)).unwrap();
        let events = posted();
        let types: Vec<_> = events.iter().map(|e| ty(e)).collect();
        use CGEventType as T;
        assert_eq!(
            types,
            [T::FlagsChanged, T::KeyDown, T::KeyUp, T::FlagsChanged]
        );
        assert_eq!(int(&events[1], CGEventField::KeyboardEventKeycode), 0x08);
        assert_eq!(flags(&events[0]) & command, command);
        assert_eq!(flags(&events[1]) & command, command);
        assert_eq!(flags(&events[2]) & command, command);
        assert_eq!(flags(&events[3]) & command, 0, "Command is up");
    }

    #[test]
    fn keys_with_no_mac_keycode_are_skipped() {
        let mut injector = recording_injector();
        injector.inject(&key(0x46, true)).unwrap(); // Print Screen
        assert!(posted().is_empty());
    }

    #[test]
    fn auto_repeat_is_marked() {
        let mut injector = recording_injector();
        let repeat = InputEvent::Key {
            code: KeyCode::keyboard(0x04),
            pressed: true,
            repeat: true,
        };
        injector.inject(&repeat).unwrap();
        assert_eq!(int(&posted()[0], CGEventField::KeyboardEventAutorepeat), 1);
    }

    #[test]
    fn scrolling_round_trips_through_capture() {
        let mut injector = recording_injector();
        injector.move_cursor_to(Point::new(50.0, 50.0)).unwrap();
        let line = InputEvent::Scroll {
            dx: 1.0,
            dy: -2.0,
            unit: ScrollUnit::Line,
        };
        let pixel = InputEvent::Scroll {
            dx: -5.0,
            dy: 12.0,
            unit: ScrollUnit::Pixel,
        };
        injector.inject(&line).unwrap();
        injector.inject(&pixel).unwrap();
        let events = posted();
        let mut converter = Converter::new(None);
        let back: Vec<_> = events[1..]
            .iter()
            .flat_map(|e| converter.convert(ty(e), e).events().collect::<Vec<_>>())
            .collect();
        assert_eq!(back, [line, pixel]);
    }

    #[test]
    fn release_all_lets_go_of_buttons_then_keys_then_modifiers() {
        let mut injector = recording_injector();
        injector.move_cursor_to(Point::new(10.0, 10.0)).unwrap();
        injector.inject(&key(0xE1, true)).unwrap(); // Left Shift: kVK_Shift
        injector.inject(&key(0x04, true)).unwrap(); // A: kVK_ANSI_A
        injector.inject(&click(MouseButton::Left, true)).unwrap();
        posted();
        injector.release_all().unwrap();
        let events = posted();
        let types: Vec<_> = events.iter().map(|e| ty(e)).collect();
        use CGEventType as T;
        assert_eq!(types, [T::LeftMouseUp, T::KeyUp, T::FlagsChanged]);
        assert_eq!(int(&events[1], CGEventField::KeyboardEventKeycode), 0x00);
        assert_ne!(
            flags(&events[1]) & CGEventFlags::MaskShift.0,
            0,
            "A's up still has Shift"
        );
        assert_eq!(int(&events[2], CGEventField::KeyboardEventKeycode), 0x38);
        assert_eq!(flags(&events[2]) & CGEventFlags::MaskShift.0, 0);
        injector.release_all().unwrap();
        assert!(posted().is_empty(), "nothing is held any more");
    }

    #[test]
    fn dropping_the_injector_releases_what_it_holds() {
        let mut injector = recording_injector();
        injector.inject(&key(0x04, true)).unwrap();
        posted();
        drop(injector);
        let events = posted();
        assert_eq!(events.len(), 1);
        assert_eq!(ty(&events[0]), CGEventType::KeyUp);
    }

    #[test]
    fn presses_close_in_time_and_place_count_up() {
        let mut clicks = ClickCounter::default();
        let interval = Duration::from_millis(500);
        let t = Instant::now();
        let at = Point::new(100.0, 100.0);
        assert_eq!(clicks.press(MouseButton::Left, at, t, interval), 1);
        let near = Point::new(102.0, 101.0);
        assert_eq!(
            clicks.press(
                MouseButton::Left,
                near,
                t + Duration::from_millis(200),
                interval
            ),
            2
        );
        assert_eq!(
            clicks.press(
                MouseButton::Left,
                near,
                t + Duration::from_millis(400),
                interval
            ),
            3
        );
        // Too slow: a new single click.
        assert_eq!(
            clicks.press(
                MouseButton::Left,
                near,
                t + Duration::from_millis(1000),
                interval
            ),
            1
        );
    }

    #[test]
    fn another_button_or_a_distant_press_starts_over() {
        let mut clicks = ClickCounter::default();
        let interval = Duration::from_millis(500);
        let t = Instant::now();
        let at = Point::new(10.0, 10.0);
        assert_eq!(clicks.press(MouseButton::Left, at, t, interval), 1);
        assert_eq!(
            clicks.press(
                MouseButton::Right,
                at,
                t + Duration::from_millis(100),
                interval
            ),
            1
        );
        assert_eq!(
            clicks.press(
                MouseButton::Right,
                at,
                t + Duration::from_millis(200),
                interval
            ),
            2
        );
        let far = Point::new(30.0, 10.0);
        assert_eq!(
            clicks.press(
                MouseButton::Right,
                far,
                t + Duration::from_millis(300),
                interval
            ),
            1
        );
    }

    #[test]
    fn event_numbers_count_up_through_the_high_range_and_wrap() {
        let mut numbers = EventNumbers::seeded(5.0);
        assert_eq!(numbers.take(), 0x4005);
        assert_eq!(numbers.take(), 0x4006);

        let mut numbers = EventNumbers::seeded(f64::NAN);
        assert_eq!(numbers.take(), 0x4000);

        // An uptime past the range's size still lands inside it.
        let mut numbers = EventNumbers::seeded((0x4000 * 3 + 0x3FFF) as f64);
        assert_eq!(numbers.take(), 0x7FFF);
        assert_eq!(numbers.take(), 0x4000, "wraps within the high range");
        for _ in 0..0x10000 {
            let n = numbers.take();
            assert!((0x4000..=0x7FFF).contains(&n), "{n:#x} fits an SInt16");
        }
    }

    #[test]
    fn motion_deltas_follow_the_whole_point_under_the_cursor() {
        let delta = |a: (f64, f64), b: (f64, f64)| {
            whole_point_delta(Point::new(a.0, a.1), Point::new(b.0, b.1))
        };
        assert_eq!(delta((100.0, 100.0), (103.0, 98.0)), (3, -2));
        // Small moves add up to the distance actually travelled.
        let steps = [100.0, 100.4, 100.8, 101.2, 101.6];
        let total: i64 = steps
            .windows(2)
            .map(|w| delta((w[0], 0.0), (w[1], 0.0)).0)
            .sum();
        assert_eq!(total, 2);
        assert_eq!(delta((0.0, 0.0), (f64::NAN, f64::INFINITY)), (0, 0));
    }

    #[test]
    fn remainders_add_up_fractions_and_cap_bursts() {
        let mut remainder = Remainder::default();
        assert_eq!(remainder.take(0.4, -0.4, 100.0), (0, 0));
        assert_eq!(remainder.take(0.4, -0.4, 100.0), (0, 0));
        assert_eq!(remainder.take(0.4, -0.4, 100.0), (1, -1));
        assert_eq!(remainder.take(2.5, 0.0, 100.0), (2, 0));
        assert_eq!(remainder.take(1e9, f64::NAN, 100.0), (100, 0));
    }

    #[test]
    fn flags_follow_the_injected_modifiers_not_the_created_event() {
        // An event created while the private source thought Command was
        // down, injected with only Left Shift held.
        let created = CGEventFlags(CGEventFlags::MaskCommand.0 | 0x08 | 0x2000_0000);
        let mut held = ModifierSet::default();
        held.set(0x38, true); // kVK_Shift
        let expected =
            CGEventFlags::MaskShift.0 | 0x02 | CGEventFlags::MaskNumericPad.0 | 0x2000_0000;
        let flags = event_flags(created, held, CGEventFlags::MaskNumericPad, false);
        assert_eq!(flags.0, expected);
        // Caps Lock comes from the Mac's own state, never the created event.
        let created = CGEventFlags(created.0 | CGEventFlags::MaskAlphaShift.0);
        assert_eq!(
            event_flags(created, held, CGEventFlags::MaskNumericPad, false).0,
            expected
        );
        assert_eq!(
            event_flags(created, held, CGEventFlags::MaskNumericPad, true).0,
            expected | CGEventFlags::MaskAlphaShift.0
        );
    }

    #[test]
    fn button_numbers_and_types_match_quartz() {
        assert_eq!(button_number(MouseButton::Middle), 2);
        assert_eq!(button_number(MouseButton::Forward), 4);
        assert_eq!(
            button_type(MouseButton::Back, true),
            CGEventType::OtherMouseDown
        );
        assert_eq!(
            button_type(MouseButton::Right, false),
            CGEventType::RightMouseUp
        );
        assert_eq!(drag_type(MouseButton::Left), CGEventType::LeftMouseDragged);
        assert_eq!(
            drag_type(MouseButton::Forward),
            CGEventType::OtherMouseDragged
        );
    }
}
