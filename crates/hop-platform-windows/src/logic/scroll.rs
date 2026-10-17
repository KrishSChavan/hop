//! Wheel units: reading them from hooks and producing them for `SendInput`.

use hop_types::ScrollUnit;

/// One wheel notch in Windows' units (`WHEEL_DELTA`).
pub const WHEEL_DELTA: f64 = 120.0;

/// How many logical units of smooth (pixel) scrolling make one wheel notch
/// when injecting. Windows has no pixel scrolling: apps scroll about three
/// lines, roughly 50 to 100 pixels, per notch. This is a starting point to
/// tune on real hardware.
pub const PIXELS_PER_NOTCH: f64 = 60.0;

/// The largest wheel amount sent in one event, so a bogus delta cannot send
/// a page flying (100 notches).
const MAX_WHEEL_UNITS: f64 = 12_000.0;

/// Notches from a hook's `mouseData` for `WM_MOUSEWHEEL` or
/// `WM_MOUSEHWHEEL`: the high word is a signed multiple of 120, or a
/// fraction of it for smooth wheels and touchpads.
///
/// The signs already match `hop_types::InputEvent::Scroll`: a positive
/// vertical value means the wheel moved forward (away from the user), a
/// positive horizontal value means scroll right.
pub fn wheel_notches(mouse_data: u32) -> f64 {
    // The high word, reinterpreted as signed.
    let delta = (mouse_data >> 16) as u16 as i16;
    f64::from(delta) / WHEEL_DELTA
}

/// Turns scroll amounts into whole wheel units for `SendInput`, carrying the
/// fractions over so slow smooth scrolling is not lost to rounding.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WheelAccumulator {
    x: f64,
    y: f64,
}

impl WheelAccumulator {
    /// The horizontal and vertical wheel amounts (in 1/120 notch) to send
    /// for a scroll of `dx`, `dy` in `unit`. Positive vertical is forward
    /// (`MOUSEEVENTF_WHEEL`), positive horizontal is right
    /// (`MOUSEEVENTF_HWHEEL`), the same convention as `InputEvent::Scroll`.
    pub fn take(&mut self, dx: f64, dy: f64, unit: ScrollUnit) -> (i32, i32) {
        let per_unit = match unit {
            ScrollUnit::Line => WHEEL_DELTA,
            ScrollUnit::Pixel => WHEEL_DELTA / PIXELS_PER_NOTCH,
        };
        (
            step(&mut self.x, dx * per_unit),
            step(&mut self.y, dy * per_unit),
        )
    }

    /// Drops any carried-over fraction.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Adds `amount` to the carried `fraction` and returns the whole part.
fn step(fraction: &mut f64, amount: f64) -> i32 {
    if !amount.is_finite() {
        return 0;
    }
    // A change of direction drops the leftover from the old direction.
    if amount * *fraction < 0.0 {
        *fraction = 0.0;
    }
    let total = (*fraction + amount).clamp(-MAX_WHEEL_UNITS, MAX_WHEEL_UNITS);
    let whole = total.trunc();
    *fraction = total - whole;
    // Within ±12000 after the clamp, so the cast is exact.
    whole as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(delta: i16) -> u32 {
        u32::from(delta as u16) << 16
    }

    #[test]
    fn hook_wheel_data_becomes_notches_with_the_contracts_signs() {
        assert_eq!(wheel_notches(data(120)), 1.0);
        assert_eq!(wheel_notches(data(-240)), -2.0);
        assert_eq!(wheel_notches(data(30)), 0.25);
        // The low word is not part of the delta.
        assert_eq!(wheel_notches(data(-120) | 0xFFFF), -1.0);
        assert_eq!(wheel_notches(0), 0.0);
    }

    #[test]
    fn lines_become_multiples_of_wheel_delta() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.take(0.0, 1.0, ScrollUnit::Line), (0, 120));
        assert_eq!(wheel.take(-2.0, 0.0, ScrollUnit::Line), (-240, 0));
        assert_eq!(wheel.take(0.0, 0.25, ScrollUnit::Line), (0, 30));
    }

    #[test]
    fn small_smooth_scrolls_accumulate() {
        let mut wheel = WheelAccumulator::default();
        // A quarter pixel is half a wheel unit: nothing yet, then one unit.
        assert_eq!(wheel.take(0.0, 0.25, ScrollUnit::Pixel), (0, 0));
        assert_eq!(wheel.take(0.0, 0.25, ScrollUnit::Pixel), (0, 1));
        // A full notch's worth of pixels is a full notch.
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            wheel.take(PIXELS_PER_NOTCH, -PIXELS_PER_NOTCH, ScrollUnit::Pixel),
            (120, -120)
        );
    }

    #[test]
    fn reversing_drops_the_old_fraction() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.take(0.0, 0.4, ScrollUnit::Pixel), (0, 0));
        // Without the reset, 0.8 - 0.4 would leave 0.4 behind and the
        // next small down-scroll would come out as nothing.
        assert_eq!(wheel.take(0.0, -0.4, ScrollUnit::Pixel), (0, 0));
        assert_eq!(wheel.take(0.0, -0.2, ScrollUnit::Pixel), (0, -1));
        wheel.reset();
        assert_eq!(wheel, WheelAccumulator::default());
    }

    #[test]
    fn absurd_amounts_are_clamped_or_ignored() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            wheel.take(f64::NAN, f64::INFINITY, ScrollUnit::Line),
            (0, 0)
        );
        assert_eq!(wheel.take(0.0, 1e9, ScrollUnit::Line), (0, 12_000));
    }
}
