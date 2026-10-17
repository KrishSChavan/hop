//! Turning what Windows reports about monitors into the contract's
//! `DisplayInfo`.

use hop_types::{DisplayInfo, Rect};

use super::coords::{PhysRect, scale_from_dpi};

/// One monitor as Windows reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    /// The GDI device name (`\\.\DISPLAY1`), UTF-16, without the NUL.
    pub device: Vec<u16>,
    /// `rcMonitor`: the monitor's area in physical pixels.
    pub rect: PhysRect,
    /// Effective DPI (`MDT_EFFECTIVE_DPI`); 96 is 100% scaling.
    pub dpi: u32,
    /// Windows' primary monitor (`MONITORINFOF_PRIMARY`).
    pub primary: bool,
}

/// The primary monitor's scale, which turns every physical coordinate into
/// logical units. 1.0 if no monitor is marked primary.
pub fn primary_scale(monitors: &[Monitor]) -> f64 {
    monitors
        .iter()
        .find(|m| m.primary)
        .map_or(1.0, |m| scale_from_dpi(m.dpi))
}

/// A stable id for a monitor: the 32-bit FNV-1a hash of its device name.
///
/// The name stays the same while the monitor stays connected, and unlike an
/// enumeration index it does not change when another monitor is unplugged.
/// FNV is fixed by its spec, so the id is the same in every build.
pub fn display_id(device: &[u16]) -> u32 {
    const OFFSET_BASIS: u32 = 0x811C_9DC5;
    const PRIME: u32 = 0x0100_0193;
    device
        .iter()
        .take_while(|&&unit| unit != 0)
        .flat_map(|unit| unit.to_le_bytes())
        .fold(OFFSET_BASIS, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(PRIME)
        })
}

/// The monitors as the contract describes displays: frames in logical units
/// (physical pixels divided by the primary scale, so touching monitors keep
/// touching), the primary first, then top to bottom and left to right.
///
/// `DisplayInfo::scale` is each monitor's own scale (its DPI over 96), as
/// Windows Settings shows it. Note that because the whole desktop is
/// divided by the *primary* scale, one logical unit is `primary_scale`
/// physical pixels on every monitor; the per-monitor value is informational.
pub fn logical_displays(monitors: &[Monitor]) -> Vec<DisplayInfo> {
    let scale = primary_scale(monitors);
    let mut sorted: Vec<&Monitor> = monitors.iter().filter(|m| !m.rect.is_empty()).collect();
    sorted.sort_by_key(|m| (!m.primary, m.rect.top, m.rect.left));
    sorted
        .into_iter()
        .map(|m| DisplayInfo {
            id: display_id(&m.device),
            frame: Rect::new(
                f64::from(m.rect.left) / scale,
                f64::from(m.rect.top) / scale,
                f64::from(m.rect.width()) / scale,
                f64::from(m.rect.height()) / scale,
            ),
            scale: scale_from_dpi(m.dpi),
            primary: m.primary,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn monitor(name: &str, rect: PhysRect, dpi: u32, primary: bool) -> Monitor {
        Monitor {
            device: wide(name),
            rect,
            dpi,
            primary,
        }
    }

    #[test]
    fn frames_are_physical_pixels_over_the_primary_scale() {
        // A 125% laptop panel with a 100% monitor to its right.
        let monitors = [
            monitor(
                r"\\.\DISPLAY2",
                PhysRect::new(1920, -200, 4480, 1240),
                96,
                false,
            ),
            monitor(r"\\.\DISPLAY1", PhysRect::new(0, 0, 1920, 1080), 120, true),
        ];
        let displays = logical_displays(&monitors);
        assert_eq!(displays.len(), 2);
        assert!(displays[0].primary);
        assert_eq!(displays[0].frame, Rect::new(0.0, 0.0, 1536.0, 864.0));
        assert_eq!(displays[0].scale, 1.25);
        assert_eq!(displays[1].frame, Rect::new(1536.0, -160.0, 2048.0, 1152.0));
        assert_eq!(displays[1].scale, 1.0);
        // Still touching: the primary's right edge is the other's left edge.
        assert_eq!(displays[0].frame.max_x(), displays[1].frame.min_x());
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let one = display_id(&wide(r"\\.\DISPLAY1"));
        assert_eq!(one, display_id(&wide(r"\\.\DISPLAY1")));
        assert_ne!(one, display_id(&wide(r"\\.\DISPLAY2")));
        // Anything after a NUL is padding from the fixed-size Win32 buffer.
        let mut padded = wide(r"\\.\DISPLAY1");
        padded.extend([0, 0x41, 0x42]);
        assert_eq!(display_id(&padded), one);
        // FNV-1a of nothing is its offset basis.
        assert_eq!(display_id(&[]), 0x811C_9DC5);
    }

    #[test]
    fn without_a_primary_the_scale_is_one_and_empty_monitors_are_dropped() {
        let monitors = [
            monitor("A", PhysRect::new(0, 0, 100, 100), 144, false),
            monitor("B", PhysRect::new(100, 0, 100, 100), 96, false),
        ];
        assert_eq!(primary_scale(&monitors), 1.0);
        let displays = logical_displays(&monitors);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].frame, Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(displays[0].scale, 1.5);
        assert!(logical_displays(&[]).is_empty());
    }
}
