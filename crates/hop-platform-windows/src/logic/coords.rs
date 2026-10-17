//! Physical pixels, logical units, and the normalized coordinates of
//! `SendInput`.
//!
//! Hop is per-monitor DPI aware, so Windows hands it physical pixels in
//! virtual-screen coordinates (the primary display's top-left is (0, 0)).
//! The contract's logical units are physical pixels divided by the *primary*
//! display's scale, so the whole desktop scales uniformly and monitors that
//! touch physically still touch logically.

use hop_types::Point;

/// A point in physical pixels, in virtual-screen coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysPoint {
    /// Pixels right of the primary display's left edge.
    pub x: i32,
    /// Pixels below the primary display's top edge.
    pub y: i32,
}

impl PhysPoint {
    /// The point `(x, y)`.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Squared distance to `other`, without overflow.
    pub fn distance_squared(self, other: PhysPoint) -> i128 {
        let dx = i128::from(self.x) - i128::from(other.x);
        let dy = i128::from(self.y) - i128::from(other.y);
        dx * dx + dy * dy
    }
}

/// A rectangle in physical pixels, laid out like a Win32 `RECT`: left and
/// top are inclusive, right and bottom exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysRect {
    /// First column.
    pub left: i32,
    /// First row.
    pub top: i32,
    /// One past the last column.
    pub right: i32,
    /// One past the last row.
    pub bottom: i32,
}

impl PhysRect {
    /// The rectangle with the given edges.
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Width in pixels (0 for an empty or inverted rectangle).
    pub fn width(&self) -> i32 {
        self.right.saturating_sub(self.left).max(0)
    }

    /// Height in pixels (0 for an empty or inverted rectangle).
    pub fn height(&self) -> i32 {
        self.bottom.saturating_sub(self.top).max(0)
    }

    /// Whether the rectangle holds no pixel.
    pub fn is_empty(&self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// Whether pixel `p` lies inside.
    pub fn contains(&self, p: PhysPoint) -> bool {
        p.x >= self.left && p.x < self.right && p.y >= self.top && p.y < self.bottom
    }

    /// The pixel at the middle, rounded toward the top-left, so it lies
    /// inside every non-empty rectangle.
    pub fn center(&self) -> PhysPoint {
        let mid = |lo: i32, hi: i32| {
            let lo = i64::from(lo);
            let hi = i64::from(hi);
            // `hi - 1` is the last pixel; the floor of the midpoint of the
            // first and last pixel is inside. The result lies between two
            // i32 values, so it fits.
            i32::try_from((lo + (hi - 1).max(lo)).div_euclid(2)).unwrap_or(0)
        };
        PhysPoint::new(mid(self.left, self.right), mid(self.top, self.bottom))
    }
}

/// Windows' baseline DPI: 100% scaling.
pub const BASE_DPI: u32 = 96;

/// The scale for a monitor DPI (96 is 1.0, 120 is 1.25). An unknown DPI (0)
/// counts as 100%.
pub fn scale_from_dpi(dpi: u32) -> f64 {
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / f64::from(BASE_DPI)
    }
}

/// `scale`, or 1.0 if it is not a usable scale (zero, negative or not finite),
/// so a bad reading can never divide by zero.
pub fn sane_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// A physical position in logical units.
pub fn to_logical(p: PhysPoint, scale: f64) -> Point {
    let scale = sane_scale(scale);
    Point::new(f64::from(p.x) / scale, f64::from(p.y) / scale)
}

/// A physical distance (a motion delta) in logical units.
pub fn length_to_logical(pixels: f64, scale: f64) -> f64 {
    pixels / sane_scale(scale)
}

/// How close to a whole pixel a scaled coordinate must be to count as that
/// pixel. Converting a display edge to logical units and back can land a
/// hair below the whole number (1097.142857 × 1.75 = 1919.9999999999998);
/// without this, flooring would put the point on the wrong display.
const SNAP: f64 = 1e-6;

/// The physical pixel that contains logical coordinate `v`.
///
/// Pixels are floored, not rounded, to match the half-open edges of
/// `hop_types::Rect::contains`: a logical point inside a display always maps
/// to a pixel inside that display, including the last column, where
/// rounding would spill onto the next display. Out-of-range values saturate.
pub fn coord_to_physical(v: f64, scale: f64) -> i32 {
    let p = v * sane_scale(scale);
    if p.is_nan() {
        return 0;
    }
    let nearest = p.round();
    let pixel = if (p - nearest).abs() < SNAP {
        nearest
    } else {
        p.floor()
    };
    // `as` saturates for out-of-range floats, which is what we want here.
    pixel as i32
}

/// The physical pixel that contains logical point `p`.
pub fn to_physical(p: Point, scale: f64) -> PhysPoint {
    PhysPoint::new(coord_to_physical(p.x, scale), coord_to_physical(p.y, scale))
}

/// The largest normalized absolute coordinate `SendInput` accepts.
pub const NORMALIZED_MAX: i32 = 65_535;

/// The normalized absolute coordinate (0 to 65535) that `SendInput` maps to
/// pixel `pixel` of an axis starting at `origin` and `extent` pixels long
/// (the virtual screen, with `MOUSEEVENTF_VIRTUALDESK`). Pixels outside the
/// axis are clamped onto it first.
///
/// Windows does not document its exact rounding, and the plausible formulas
/// disagree by a pixel: `floor(n * extent / 65536)`, `round(n * extent /
/// 65536)` and `round(n * (extent - 1) / 65535)`. Each pixel owns a run of
/// normalized values; this picks the value just below the middle of the
/// run, which maps back to the same pixel under all three, including the
/// first and last pixel of the axis. A test checks that for every pixel of
/// common screen widths.
pub fn normalize(pixel: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 0 {
        return 0;
    }
    let extent = i64::from(extent);
    let offset = (i64::from(pixel) - i64::from(origin)).clamp(0, extent - 1);
    // The run for pixel k is [k * 65536 / extent, (k + 1) * 65536 / extent);
    // its middle is (2k + 1) * 32768 / extent. Take the largest integer
    // strictly below the middle: ceil(middle) - 1.
    let numerator = (2 * offset + 1) * 32_768;
    let below_middle = (numerator + extent - 1) / extent - 1;
    // Clamped into 0..=65535, so it fits in an i32.
    i32::try_from(below_middle.clamp(0, i64::from(NORMALIZED_MAX))).unwrap_or(NORMALIZED_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three mappings Windows might use, from a normalized value back to
    /// a pixel offset.
    fn floor_65536(n: i32, extent: i32) -> i64 {
        (i64::from(n) * i64::from(extent)) >> 16
    }

    fn round_65536(n: i32, extent: i32) -> i64 {
        (i64::from(n) * i64::from(extent) + 32_768) >> 16
    }

    fn round_65535(n: i32, extent: i32) -> i64 {
        let num = i64::from(n) * (i64::from(extent) - 1);
        (num * 2 + 65_535) / (2 * 65_535)
    }

    #[test]
    fn normalized_points_map_back_to_their_pixel_under_every_plausible_rounding() {
        for extent in [
            1, 2, 3, 640, 1024, 1280, 1366, 1440, 1536, 1600, 1920, 2048, 2256, 2560, 2880, 3000,
            3200, 3440, 3840, 4096, 5120, 5760, 7680,
        ] {
            for pixel in 0..extent {
                let n = normalize(pixel, 0, extent);
                assert!((0..=NORMALIZED_MAX).contains(&n));
                for (name, back) in [
                    ("floor/65536", floor_65536(n, extent)),
                    ("round/65536", round_65536(n, extent)),
                    ("round/65535", round_65535(n, extent)),
                ] {
                    assert_eq!(
                        back,
                        i64::from(pixel),
                        "{name}: pixel {pixel} of {extent} -> {n} -> {back}"
                    );
                }
            }
        }
    }

    #[test]
    fn normalization_is_relative_to_the_virtual_screen_origin_and_clamps() {
        // A monitor left of the primary puts the virtual screen's origin at
        // a negative x.
        let (origin, extent) = (-1920, 3840);
        assert_eq!(normalize(-1920, origin, extent), normalize(0, 0, extent));
        assert_eq!(normalize(0, origin, extent), normalize(1920, 0, extent));
        assert_eq!(
            normalize(-5000, origin, extent),
            normalize(-1920, origin, extent)
        );
        assert_eq!(
            normalize(9999, origin, extent),
            normalize(1919, origin, extent)
        );
        assert_eq!(normalize(5, 0, 0), 0);
    }

    #[test]
    fn logical_points_floor_onto_the_pixel_that_contains_them() {
        // 125%: a 1920-pixel display is 1536 units wide.
        assert_eq!(coord_to_physical(0.0, 1.25), 0);
        assert_eq!(coord_to_physical(1535.0, 1.25), 1918);
        assert_eq!(coord_to_physical(1535.9, 1.25), 1919);
        assert_eq!(coord_to_physical(1536.0, 1.25), 1920);
        // Just left of the primary display belongs to the display there.
        assert_eq!(coord_to_physical(-0.1, 1.25), -1);
        assert_eq!(coord_to_physical(-1.0, 1.0), -1);
    }

    #[test]
    fn edges_survive_the_round_trip_through_logical_units() {
        // 175%: 1920 / 1.75 is not exact in binary.
        let scale = 1.75;
        let edge = to_logical(PhysPoint::new(1920, 1080), scale);
        assert_eq!(to_physical(edge, scale), PhysPoint::new(1920, 1080));
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 3.0, 3.5] {
            for px in [-2560, -1, 0, 1, 1365, 1919, 1920, 2559, 3839] {
                let logical = to_logical(PhysPoint::new(px, px), scale);
                assert_eq!(
                    to_physical(logical, scale),
                    PhysPoint::new(px, px),
                    "{px} at {scale}"
                );
            }
        }
    }

    #[test]
    fn bad_values_never_divide_by_zero_or_wrap() {
        assert_eq!(sane_scale(0.0), 1.0);
        assert_eq!(sane_scale(-2.0), 1.0);
        assert_eq!(sane_scale(f64::NAN), 1.0);
        assert_eq!(
            to_logical(PhysPoint::new(10, 20), 0.0),
            Point::new(10.0, 20.0)
        );
        assert_eq!(coord_to_physical(f64::NAN, 1.0), 0);
        assert_eq!(coord_to_physical(1e300, 1.0), i32::MAX);
        assert_eq!(coord_to_physical(-1e300, 1.0), i32::MIN);
        assert_eq!(scale_from_dpi(0), 1.0);
        assert_eq!(scale_from_dpi(120), 1.25);
        assert_eq!(scale_from_dpi(144), 1.5);
        assert_eq!(length_to_logical(15.0, 1.5), 10.0);
    }

    #[test]
    fn rectangles_follow_win32_conventions() {
        let r = PhysRect::new(0, 0, 1920, 1080);
        assert!(r.contains(PhysPoint::new(0, 0)));
        assert!(r.contains(PhysPoint::new(1919, 1079)));
        assert!(!r.contains(PhysPoint::new(1920, 0)));
        assert_eq!(r.center(), PhysPoint::new(959, 539));
        assert_eq!(PhysRect::new(-1, -1, 0, 0).center(), PhysPoint::new(-1, -1));
        assert!(PhysRect::new(5, 5, 5, 9).is_empty());
        assert_eq!(PhysRect::new(5, 5, 1, 1).width(), 0);
        assert_eq!(
            PhysPoint::new(i32::MIN, 0).distance_squared(PhysPoint::new(i32::MAX, 0)),
            (i128::from(u32::MAX)) * (i128::from(u32::MAX))
        );
    }
}
