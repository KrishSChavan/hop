//! The Mac's displays, in Quartz global coordinates.

use hop_platform::{Displays, PlatformError, Result};
use hop_types::{DisplayInfo, Point, Rect};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMirrorsDisplay,
    CGDisplayMode, CGError, CGGetActiveDisplayList, CGMainDisplayID, kCGNullDirectDisplay,
};

/// More active displays than any Mac drives.
const MAX_DISPLAYS: usize = 32;

/// [`Displays`] for macOS.
pub(crate) struct MacDisplays;

impl Displays for MacDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        active_displays()
    }
}

/// Every active display. Frames are Quartz global coordinates in points,
/// which is exactly Hop's logical space on a Mac: the origin is the main
/// display's top-left corner and y grows downward. A display that mirrors
/// another is left out, since it shows the same pixels as the one it
/// mirrors.
pub(crate) fn active_displays() -> Result<Vec<DisplayInfo>> {
    let mut ids: [CGDirectDisplayID; MAX_DISPLAYS] = [0; MAX_DISPLAYS];
    let mut count: u32 = 0;
    // SAFETY: `ids` has room for MAX_DISPLAYS entries and `count` is a valid
    // place for the result; the call writes nothing beyond either.
    let error =
        unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    check(error, "CGGetActiveDisplayList")?;
    let main = CGMainDisplayID();
    let found = ids.get(..count as usize).unwrap_or(&ids);
    Ok(found
        .iter()
        .copied()
        .filter(|&id| CGDisplayMirrorsDisplay(id) == kCGNullDirectDisplay)
        .map(|id| {
            let frame = bounds(id);
            DisplayInfo {
                id,
                frame,
                scale: scale(id, frame.size.width),
                primary: id == main,
            }
        })
        .collect())
}

/// A display's frame in Quartz global coordinates.
fn bounds(id: CGDirectDisplayID) -> Rect {
    let b = CGDisplayBounds(id);
    Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height)
}

/// The main display's frame: the one whose top-left corner is the origin
/// and that has the menu bar.
pub(crate) fn main_display_frame() -> Rect {
    bounds(CGMainDisplayID())
}

/// The middle of the main display, which is away from the menu bar, the
/// Dock and every hot corner.
pub(crate) fn main_display_center() -> Point {
    main_display_frame().center()
}

/// Physical pixels per point for display `id` (2.0 on Retina).
fn scale(id: CGDirectDisplayID, point_width: f64) -> f64 {
    let Some(mode) = CGDisplayCopyDisplayMode(id) else {
        return 1.0;
    };
    pixel_scale(CGDisplayMode::pixel_width(Some(&mode)) as f64, point_width)
}

/// Pixels per point, or 1.0 when either width is unknown.
pub(crate) fn pixel_scale(pixel_width: f64, point_width: f64) -> f64 {
    if pixel_width > 0.0 && point_width > 0.0 {
        pixel_width / point_width
    } else {
        1.0
    }
}

/// Turns a Quartz error code into a [`PlatformError`].
pub(crate) fn check(error: CGError, call: &str) -> Result<()> {
    if error == CGError::Success {
        Ok(())
    } else {
        Err(PlatformError::Os(format!(
            "{call} failed with error {}",
            error.0
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_is_pixels_per_point() {
        assert_eq!(pixel_scale(3024.0, 1512.0), 2.0);
        assert_eq!(pixel_scale(1920.0, 1920.0), 1.0);
        assert_eq!(pixel_scale(0.0, 1512.0), 1.0);
        assert_eq!(pixel_scale(3024.0, 0.0), 1.0);
    }

    #[test]
    fn errors_name_the_call() {
        assert!(check(CGError::Success, "X").is_ok());
        let error = check(CGError::IllegalArgument, "CGWarpMouseCursorPosition").unwrap_err();
        assert_eq!(
            error.to_string(),
            "CGWarpMouseCursorPosition failed with error 1001"
        );
    }

    /// Reads the real display list. This only queries the WindowServer; it
    /// changes nothing on screen.
    #[test]
    fn the_main_display_is_listed_at_the_origin() {
        let displays = match active_displays() {
            Ok(displays) if !displays.is_empty() => displays,
            // A session without a window server (over SSH, say) has no
            // displays to check.
            other => return eprintln!("no displays to check: {other:?}"),
        };
        let primary: Vec<_> = displays.iter().filter(|d| d.primary).collect();
        assert_eq!(primary.len(), 1, "{displays:?}");
        let main = primary[0];
        assert_eq!(main.frame.origin, Point::new(0.0, 0.0));
        assert!(main.frame.size.width > 0.0 && main.frame.size.height > 0.0);
        assert!(main.scale >= 1.0, "{main:?}");
        assert_eq!(main_display_frame(), main.frame);
        assert!(main.frame.contains(main_display_center()));
    }
}
