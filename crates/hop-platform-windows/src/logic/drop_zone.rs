//! The drop zone's rules (milestone M5): where the windows along the edges
//! that lead to the other computer go, when a drag arms them, and what they
//! show and answer while files are dragged over them
//! (docs/M4-M5-DESIGN.md, section 5.1).
//!
//! # Where
//!
//! A zone is the part of one display's edge that leads to the other
//! computer (`hop_platform::DropZone`, in logical units). Its window lies
//! along that part of the edge, inside the display, [`ZONE_DEPTH`] deep
//! ([`window_rect`], in physical pixels, converted the way the rest of the
//! backend converts).
//!
//! # When
//!
//! A window shows only during a drag near its zone, so it never gets in the
//! way of ordinary clicks or of window snapping at the edge. The mouse hook
//! follows each left-button gesture ([`DragGesture`]): once the cursor has
//! moved past the system's drag threshold from where the button went down,
//! every zone within [`ARM_REACH`] of the cursor is armed, and the button
//! going up disarms them. The hook sees Hop's own injected buttons and moves
//! too, so a drag the other computer drives here arms the zones as well.
//! The reach is measured in logical units, like the zones, so it is more
//! than the window's depth on every display scale: the window is there
//! before the cursor reaches it.
//!
//! # What it shows ([`Target`])
//!
//! - **Hidden.**
//! - **Armed:** shown at [`ARMED_ALPHA`]: invisible, but there for drag and
//!   drop to find under the cursor.
//! - **Highlighted:** files are dragged over it: shown at
//!   [`HIGHLIGHT_ALPHA`] with its label, answering "copy" if the source
//!   allows it.
//!
//! The files leaving (or the drag being cancelled) go back to armed. A drop
//! takes the files, answers "copy" and hides. After the button goes up, the
//! window hides [`HIDE_DELAY`] later, or [`DROP_GRACE`] after that while
//! files are over it and their drop may still be on its way. The answer is
//! never "move": the files always stay where they were.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hop_platform::DropZone;
use hop_types::{Edge, Point};

use super::coords::{PhysPoint, PhysRect, coord_to_physical, to_logical};

/// How deep a zone's window reaches into the display, in logical units.
pub const ZONE_DEPTH: f64 = 120.0;

/// How near a drag must come to a zone to arm it, in logical units (150
/// pixels at 100% scaling). More than [`ZONE_DEPTH`], so the window is there
/// before the cursor reaches it.
pub const ARM_REACH: f64 = 150.0;

/// How long after the button goes up a window stays, so that the drop the
/// button-up starts, and the drag source's last moves, still find it.
pub const HIDE_DELAY: Duration = Duration::from_millis(500);

/// How much longer a highlighted window waits for its drop after
/// [`HIDE_DELAY`], before it hides anyway: the drag source drops at once, so
/// this only covers one that never finishes.
pub const DROP_GRACE: Duration = Duration::from_secs(5);

/// How often the drop thread checks, while a window shows, whether it is due
/// to hide, in milliseconds.
pub const TICK_MS: u32 = 100;

/// The most zones there can be: an arm names its zones with one bit each.
/// Zones past this are left out (a layout has a handful).
pub const MAX_ZONES: usize = 64;

/// An armed window's alpha: invisible, but unlike alpha 0 still hit-tested,
/// so drag and drop finds it.
pub const ARMED_ALPHA: u8 = 1;

/// A highlighted window's alpha, of 255.
pub const HIGHLIGHT_ALPHA: u8 = 200;

/// The most file paths one drop gives. A transfer carries at most 1,000
/// files, so a larger drop is refused whole anyway; this only bounds the
/// time and memory a drag source that claims millions can cost.
pub const MAX_DROPPED_FILES: usize = 10_000;

/// The most drops kept waiting to be collected. If nobody collects them,
/// the oldest go.
pub const MAX_WAITING_DROPS: usize = 64;

/// The part of a display's edge that a zone covers, in logical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeLine {
    /// The edge's own coordinate: x for a left or right edge, y for a top or
    /// bottom one.
    pub at: f64,
    /// Where the zone starts along the edge (y or x).
    pub from: f64,
    /// Where it ends, after `from`.
    pub to: f64,
    /// Whether the edge is a left or right one (a vertical line).
    pub vertical: bool,
}

impl EdgeLine {
    /// The part of its display's edge that `zone` covers, or `None` if it
    /// covers none of it.
    pub fn of(zone: &DropZone) -> Option<EdgeLine> {
        // `max` and `min` would quietly turn a NaN end into the display's.
        if zone.start.is_nan() || zone.end.is_nan() {
            return None;
        }
        let (span_start, span_end) = zone.display.edge_span(zone.edge);
        let from = zone.start.max(span_start);
        let to = zone.end.min(span_end);
        // Also refuses a display with NaN in its frame.
        (to > from).then(|| EdgeLine {
            at: zone.display.edge_line(zone.edge),
            from,
            to,
            vertical: zone.edge.is_vertical(),
        })
    }

    /// The squared distance from `p` to the nearest point of the line.
    pub fn distance_squared(&self, p: Point) -> f64 {
        let (across, along) = if self.vertical {
            (p.x, p.y)
        } else {
            (p.y, p.x)
        };
        let beyond = (self.from - along).max(along - self.to).max(0.0);
        let off = across - self.at;
        off * off + beyond * beyond
    }
}

/// The zones worth a window: those that cover some of their display's edge,
/// at most [`MAX_ZONES`] of them, in order.
pub fn usable_zones(zones: Vec<DropZone>) -> Vec<DropZone> {
    zones
        .into_iter()
        .filter(|zone| EdgeLine::of(zone).is_some())
        .take(MAX_ZONES)
        .collect()
}

/// Where the window of `zone` goes, in physical pixels, with `scale`
/// physical pixels per logical unit (the primary display's scale), or
/// `None` if it covers no pixel.
///
/// Logical coordinates become pixels the way the rest of the backend turns
/// them (`coords::coord_to_physical`), so a window along a display's edge
/// ends exactly at that display's last pixel. A display narrower (or
/// shorter) than [`ZONE_DEPTH`] is covered whole.
pub fn window_rect(zone: &DropZone, scale: f64) -> Option<PhysRect> {
    let line = EdgeLine::of(zone)?;
    let display = zone.display;
    let px = |v: f64| coord_to_physical(v, scale);
    let (first, past) = (px(line.from), px(line.to));
    let width = ZONE_DEPTH.min(display.size.width);
    let height = ZONE_DEPTH.min(display.size.height);
    let rect = match zone.edge {
        Edge::Left => PhysRect::new(
            px(display.min_x()),
            first,
            px(display.min_x() + width),
            past,
        ),
        Edge::Right => PhysRect::new(
            px(display.max_x() - width),
            first,
            px(display.max_x()),
            past,
        ),
        Edge::Top => PhysRect::new(
            first,
            px(display.min_y()),
            past,
            px(display.min_y() + height),
        ),
        Edge::Bottom => PhysRect::new(
            first,
            px(display.max_y() - height),
            past,
            px(display.max_y()),
        ),
    };
    (!rect.is_empty()).then_some(rect)
}

/// The zones as the mouse hook arms them.
#[derive(Clone, Debug, PartialEq)]
pub struct ZoneSet {
    /// Which setting of the zones this is. Arms name it, so an arm for
    /// zones that have been replaced since is ignored.
    pub generation: u64,
    /// Each zone's edge, in zone order: bit `i` of an arm is zone `i`.
    pub edges: Vec<EdgeLine>,
}

impl ZoneSet {
    /// No zones: nothing is ever armed.
    pub const EMPTY: ZoneSet = ZoneSet {
        generation: 0,
        edges: Vec::new(),
    };

    /// The zones of setting `generation`, which must be [`usable_zones`]
    /// so that their bits match the drop thread's windows.
    pub fn new(generation: u64, zones: &[DropZone]) -> ZoneSet {
        ZoneSet {
            generation,
            edges: zones.iter().filter_map(EdgeLine::of).collect(),
        }
    }

    /// The zones within [`ARM_REACH`] of `p` (logical units), one bit each.
    pub fn near(&self, p: Point) -> u64 {
        let reach = ARM_REACH * ARM_REACH;
        self.edges
            .iter()
            .take(MAX_ZONES)
            .enumerate()
            .filter(|(_, edge)| edge.distance_squared(p) <= reach)
            .fold(0, |zones, (index, _)| zones | 1 << index)
    }
}

/// What the mouse hook tells the drop thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Show these zones' windows (one bit each), armed.
    Arm {
        /// Every zone this gesture armed so far.
        zones: u64,
        /// The [`ZoneSet::generation`] they belong to.
        generation: u64,
    },
    /// The left button went up: hide the windows soon.
    Disarm {
        /// The [`ZoneSet::generation`] the armed windows belong to.
        generation: u64,
    },
}

/// One left-button gesture as the mouse hook sees it: press, moves, release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragGesture {
    /// Where the left button went down, while it is held.
    press: Option<PhysPoint>,
    /// The cursor has moved past the drag threshold since.
    dragging: bool,
    /// The zone set that `armed` belongs to.
    generation: u64,
    /// The zones this gesture armed, one bit each.
    armed: u64,
}

impl DragGesture {
    /// Whether the left button is held: then moves count.
    pub fn held(&self) -> bool {
        self.press.is_some()
    }

    /// The left button went down at `at`. If an earlier gesture armed zones
    /// and its button-up never came, they are disarmed first.
    pub fn press(&mut self, at: PhysPoint) -> Option<Signal> {
        let stale = self.release();
        self.press = Some(at);
        stale
    }

    /// The left button went up. Disarms the zones this gesture armed.
    pub fn release(&mut self) -> Option<Signal> {
        let signal = (self.armed != 0).then_some(Signal::Disarm {
            generation: self.generation,
        });
        *self = Self::default();
        signal
    }

    /// The cursor moved to `at` (physical pixels) with the left button held.
    /// `threshold` is the system's drag threshold (`SM_CXDRAG`,
    /// `SM_CYDRAG`): how far the cursor may move from the press, on either
    /// axis, before it is a drag. `scale` is the primary display's, which
    /// turns `at` into the zones' logical units. Arms the zones within
    /// reach that this gesture has not armed yet.
    pub fn moved(
        &mut self,
        at: PhysPoint,
        threshold: (i32, i32),
        zones: &ZoneSet,
        scale: f64,
    ) -> Option<Signal> {
        let press = self.press?;
        if !self.dragging {
            self.dragging = past_threshold(press, at, threshold);
            if !self.dragging {
                return None;
            }
        }
        if zones.generation != self.generation {
            // The zones were replaced: what this gesture armed is gone.
            self.generation = zones.generation;
            self.armed = 0;
        }
        let near = zones.near(to_logical(at, scale));
        if near & !self.armed == 0 {
            return None;
        }
        self.armed |= near;
        Some(Signal::Arm {
            zones: self.armed,
            generation: self.generation,
        })
    }
}

/// Whether `at` lies outside the drag rectangle around `press`.
fn past_threshold(press: PhysPoint, at: PhysPoint, (x, y): (i32, i32)) -> bool {
    let dx = (i64::from(at.x) - i64::from(press.x)).abs();
    let dy = (i64::from(at.y) - i64::from(press.y)).abs();
    dx > i64::from(x.max(0)) || dy > i64::from(y.max(0))
}

/// What a zone's window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Look {
    /// Not shown.
    #[default]
    Hidden,
    /// Shown at [`ARMED_ALPHA`]: invisible, but there for drag and drop.
    Armed,
    /// Shown at [`HIGHLIGHT_ALPHA`] with its label: files are dragged over
    /// it.
    Highlighted,
}

impl Look {
    /// Whether the window is on screen, visible or not.
    pub fn shown(self) -> bool {
        self != Look::Hidden
    }
}

/// The answer to the drag source: what dropping here does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing: the drop is refused.
    None,
    /// The files are copied; they stay where they are.
    Copy,
}

fn answer(copy_allowed: bool) -> Effect {
    if copy_allowed {
        Effect::Copy
    } else {
        Effect::None
    }
}

/// One zone window's state (see the module documentation). The calls follow
/// what drag and drop tells the window (`IDropTarget`), plus the hook's arm
/// and disarm and the drop thread's timer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    look: Look,
    /// When to hide, once the button went up.
    hide_at: Option<Instant>,
}

impl Target {
    /// What the window shows now.
    pub fn look(&self) -> Look {
        self.look
    }

    /// A drag came within reach: show the window, invisibly, for drag and
    /// drop to find. The drag goes on, so a pending hide is cancelled.
    pub fn arm(&mut self) {
        if self.look == Look::Hidden {
            self.look = Look::Armed;
        }
        self.hide_at = None;
    }

    /// The left button went up at `now`: hide [`HIDE_DELAY`] later (see
    /// [`Target::tick`]).
    pub fn disarm(&mut self, now: Instant) {
        if self.look.shown() {
            self.hide_at.get_or_insert(now + HIDE_DELAY);
        }
    }

    /// Something is dragged onto the window. `files`: the data holds file
    /// paths (`CF_HDROP`); `copy_allowed`: the source allows a copy.
    pub fn drag_enter(&mut self, files: bool, copy_allowed: bool) -> Effect {
        match self.look {
            // Not on screen, so not what the cursor is over: a late call.
            Look::Hidden => Effect::None,
            _ if files => {
                self.look = Look::Highlighted;
                self.hide_at = None;
                answer(copy_allowed)
            }
            _ => {
                self.look = Look::Armed;
                Effect::None
            }
        }
    }

    /// The drag moves over the window: the answer stays the one it got on
    /// entering, for what the source allows now.
    pub fn drag_over(&self, copy_allowed: bool) -> Effect {
        if self.look == Look::Highlighted {
            answer(copy_allowed)
        } else {
            Effect::None
        }
    }

    /// The drag left the window, or was cancelled.
    pub fn drag_leave(&mut self) {
        if self.look == Look::Highlighted {
            self.look = Look::Armed;
        }
    }

    /// A click landed on the window, still shown after a drag: no files are
    /// being dragged over it (a drag holds the mouse), so it hides at once
    /// and the next click goes through. A highlighted window waits for its
    /// drop instead.
    pub fn clicked(&mut self) {
        if self.look != Look::Highlighted {
            *self = Target::default();
        }
    }

    /// The drag was dropped here. `files`: paths were read from it. The
    /// window hides, whatever the answer; a drop that comes after the window
    /// hid is still taken, since the user dropped it here.
    pub fn drop_files(&mut self, files: bool, copy_allowed: bool) -> Effect {
        *self = Target::default();
        if files {
            answer(copy_allowed)
        } else {
            Effect::None
        }
    }

    /// The drop thread's timer, every [`TICK_MS`]: hides the window when it
    /// is due. `button_down`: a mouse button is held now, so a drag may go
    /// on. A shown window with no button held hides as if the button-up had
    /// been seen, so a missed one (the hooks were removed, capture stopped)
    /// cannot leave it there.
    pub fn tick(&mut self, now: Instant, button_down: bool) {
        if !self.look.shown() {
            return;
        }
        if self.hide_at.is_none() && !button_down {
            self.hide_at = Some(now + HIDE_DELAY);
        }
        let Some(at) = self.hide_at else {
            return;
        };
        let due = if self.look == Look::Highlighted {
            at + DROP_GRACE
        } else {
            at
        };
        if now >= due {
            *self = Target::default();
        }
    }
}

/// Adds one drop's paths to the drops waiting to be collected, oldest
/// first, keeping at most [`MAX_WAITING_DROPS`]: if nobody collects them,
/// the oldest go. A drop without paths is not kept.
pub fn queue_drop(waiting: &mut VecDeque<Vec<PathBuf>>, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    while waiting.len() >= MAX_WAITING_DROPS {
        waiting.pop_front();
    }
    waiting.push_back(paths);
}

#[cfg(test)]
mod tests {
    use hop_types::Rect;

    use super::*;

    fn p(x: i32, y: i32) -> PhysPoint {
        PhysPoint::new(x, y)
    }

    /// The PC's 1920x1080 panel at 125%: 1536x864 logical units.
    fn panel() -> Rect {
        Rect::new(0.0, 0.0, 1536.0, 864.0)
    }

    fn zone(display: Rect, edge: Edge, start: f64, end: f64) -> DropZone {
        DropZone {
            display,
            edge,
            start,
            end,
        }
    }

    #[test]
    fn a_left_zone_hugs_the_left_edge() {
        // The Mac on the PC's left: the edge minus 10-unit corner zones.
        let left = zone(panel(), Edge::Left, 10.0, 854.0);
        // 120 units are 150 pixels; 10 and 854 units are rows 12 and 1067.
        assert_eq!(
            window_rect(&left, 1.25),
            Some(PhysRect::new(0, 12, 150, 1067))
        );
        assert_eq!(
            EdgeLine::of(&left),
            Some(EdgeLine {
                at: 0.0,
                from: 10.0,
                to: 854.0,
                vertical: true
            })
        );
    }

    #[test]
    fn a_right_zone_ends_at_the_displays_last_column() {
        let right = zone(panel(), Edge::Right, 0.0, 864.0);
        assert_eq!(
            window_rect(&right, 1.25),
            Some(PhysRect::new(1770, 0, 1920, 1080))
        );
        assert_eq!(EdgeLine::of(&right).map(|line| line.at), Some(1536.0));
        // A display right of the primary, at 100%.
        let second = Rect::new(1536.0, -100.0, 2560.0, 1440.0);
        let zone = zone(second, Edge::Right, -100.0, 1340.0);
        assert_eq!(
            window_rect(&zone, 1.0),
            Some(PhysRect::new(3976, -100, 4096, 1340))
        );
    }

    #[test]
    fn top_and_bottom_zones_run_along_the_rows() {
        let top = zone(panel(), Edge::Top, 100.0, 300.0);
        assert_eq!(
            window_rect(&top, 1.0),
            Some(PhysRect::new(100, 0, 300, 120))
        );
        let bottom = zone(panel(), Edge::Bottom, 100.0, 300.0);
        assert_eq!(
            window_rect(&bottom, 1.0),
            Some(PhysRect::new(100, 744, 300, 864))
        );
        assert_eq!(
            EdgeLine::of(&bottom),
            Some(EdgeLine {
                at: 864.0,
                from: 100.0,
                to: 300.0,
                vertical: false
            })
        );
        // At 150%, from the same logical zone.
        assert_eq!(
            window_rect(&bottom, 1.5),
            Some(PhysRect::new(150, 1116, 450, 1296))
        );
    }

    #[test]
    fn a_zone_never_reaches_past_its_display() {
        // A display narrower than the depth: the window covers all of it.
        let narrow = Rect::new(0.0, 0.0, 80.0, 600.0);
        assert_eq!(
            window_rect(&zone(narrow, Edge::Right, 0.0, 600.0), 1.0),
            Some(PhysRect::new(0, 0, 80, 600))
        );
        // A zone running past the display's ends is cut to the display.
        let long = zone(panel(), Edge::Left, -500.0, 5000.0);
        assert_eq!(window_rect(&long, 1.0), Some(PhysRect::new(0, 0, 120, 864)));
        assert_eq!(
            EdgeLine::of(&long).map(|line| (line.from, line.to)),
            Some((0.0, 864.0))
        );
    }

    #[test]
    fn empty_or_nonsense_zones_have_no_place() {
        let none = |zone: DropZone| {
            assert_eq!(window_rect(&zone, 1.0), None, "{zone:?}");
            assert_eq!(EdgeLine::of(&zone), None, "{zone:?}");
        };
        none(zone(panel(), Edge::Left, 300.0, 300.0));
        none(zone(panel(), Edge::Left, 300.0, 200.0));
        none(zone(panel(), Edge::Left, f64::NAN, 200.0));
        none(zone(panel(), Edge::Top, 2000.0, 3000.0));
        // Less than a pixel has an edge, but no window.
        let sliver = zone(panel(), Edge::Left, 10.2, 10.6);
        assert!(EdgeLine::of(&sliver).is_some());
        assert_eq!(window_rect(&sliver, 1.0), None);
        let flat = Rect::new(0.0, 0.0, 0.0, 600.0);
        assert_eq!(window_rect(&zone(flat, Edge::Right, 0.0, 600.0), 1.0), None);
    }

    #[test]
    fn only_usable_zones_are_kept_in_order_and_at_most_64() {
        let good = zone(panel(), Edge::Right, 10.0, 854.0);
        let empty = zone(panel(), Edge::Right, 900.0, 950.0);
        let top = zone(panel(), Edge::Top, 100.0, 300.0);
        assert_eq!(
            usable_zones(vec![good.clone(), empty, top.clone()]),
            vec![good.clone(), top]
        );
        let many = usable_zones(vec![good; MAX_ZONES + 5]);
        assert_eq!(many.len(), MAX_ZONES);
        // Their edges line up with them, one bit each.
        assert_eq!(ZoneSet::new(3, &many).edges.len(), MAX_ZONES);
    }

    #[test]
    fn the_reach_is_more_than_the_depth_at_every_scale() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            assert!(
                coord_to_physical(ARM_REACH, scale) > coord_to_physical(ZONE_DEPTH, scale),
                "{scale}"
            );
        }
    }

    fn right_and_top() -> ZoneSet {
        // Zone 0 on the right edge, zone 1 along the top.
        ZoneSet::new(
            7,
            &[
                zone(panel(), Edge::Right, 10.0, 854.0),
                zone(panel(), Edge::Top, 100.0, 300.0),
            ],
        )
    }

    #[test]
    fn zones_within_reach_are_near() {
        let zones = right_and_top();
        let at = |x, y| zones.near(Point::new(x, y));
        assert_eq!(at(800.0, 500.0), 0);
        // 150 units from the right edge, and a little more.
        assert_eq!(at(1536.0 - 150.0, 500.0), 0b01);
        assert_eq!(at(1536.0 - 150.5, 500.0), 0);
        // Past the end of the zone, the distance is to its end.
        assert_eq!(at(1535.0, 854.0 + 149.0), 0b01);
        assert_eq!(at(1536.0 - 100.0, 854.0 + 120.0), 0);
        // Near both, in a corner.
        let both = ZoneSet::new(
            7,
            &[
                zone(panel(), Edge::Right, 0.0, 864.0),
                zone(panel(), Edge::Top, 0.0, 1536.0),
            ],
        );
        assert_eq!(both.near(Point::new(1500.0, 40.0)), 0b11);
        assert_eq!(at(200.0, 40.0), 0b10);
        assert_eq!(ZoneSet::EMPTY.near(Point::new(1535.0, 500.0)), 0);
    }

    #[test]
    fn only_the_first_64_zones_can_be_armed() {
        let left = zone(panel(), Edge::Left, 0.0, 100.0);
        let zones = ZoneSet {
            generation: 1,
            edges: vec![EdgeLine::of(&left).expect("an edge"); MAX_ZONES + 5],
        };
        assert_eq!(zones.near(Point::new(0.0, 50.0)), u64::MAX);
    }

    /// Gestures at 100%, with Windows' default 4-pixel threshold.
    fn moved(gesture: &mut DragGesture, at: PhysPoint, zones: &ZoneSet) -> Option<Signal> {
        gesture.moved(at, (4, 4), zones, 1.0)
    }

    #[test]
    fn a_click_never_arms_a_zone() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        // Pressed right at the edge, moved within the drag threshold.
        assert_eq!(gesture.press(p(1530, 500)), None);
        assert!(gesture.held());
        assert_eq!(moved(&mut gesture, p(1534, 503), &zones), None);
        assert_eq!(moved(&mut gesture, p(1526, 496), &zones), None);
        assert_eq!(gesture.release(), None);
        assert!(!gesture.held());
        // A move with no button held is no drag.
        assert_eq!(moved(&mut gesture, p(1535, 500), &zones), None);
    }

    #[test]
    fn a_drag_toward_a_zone_arms_it_once_and_the_release_disarms_it() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        assert_eq!(gesture.press(p(700, 500)), None);
        // A drag, but far from every zone.
        assert_eq!(moved(&mut gesture, p(900, 500), &zones), None);
        // Within reach of the right edge.
        assert_eq!(
            moved(&mut gesture, p(1400, 500), &zones),
            Some(Signal::Arm {
                zones: 0b01,
                generation: 7
            })
        );
        // Further moves near the same zone say nothing new.
        assert_eq!(moved(&mut gesture, p(1500, 520), &zones), None);
        assert_eq!(moved(&mut gesture, p(900, 520), &zones), None);
        assert_eq!(moved(&mut gesture, p(1535, 520), &zones), None);
        // Then along to the top zone: both are armed now.
        assert_eq!(
            moved(&mut gesture, p(250, 20), &zones),
            Some(Signal::Arm {
                zones: 0b11,
                generation: 7
            })
        );
        assert_eq!(gesture.release(), Some(Signal::Disarm { generation: 7 }));
        assert_eq!(gesture.release(), None);
    }

    #[test]
    fn the_reach_follows_the_primary_scale() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        // At 125% the right edge is at pixel 1920; 150 units are 187.5
        // pixels, so 1700 is out of reach and 1740 within it.
        gesture.press(p(1000, 500));
        assert_eq!(gesture.moved(p(1700, 500), (4, 4), &zones, 1.25), None);
        assert_eq!(
            gesture.moved(p(1740, 500), (4, 4), &zones, 1.25),
            Some(Signal::Arm {
                zones: 0b01,
                generation: 7
            })
        );
    }

    #[test]
    fn the_threshold_counts_from_the_press_on_either_axis() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        gesture.press(p(1450, 500));
        // Right where the button went down is within reach, but not a drag.
        assert_eq!(moved(&mut gesture, p(1450, 504), &zones), None);
        assert_eq!(
            moved(&mut gesture, p(1450, 505), &zones),
            Some(Signal::Arm {
                zones: 0b01,
                generation: 7
            })
        );
        // Once a drag, always a drag: coming back near the press point
        // still counts.
        gesture.release();
        gesture.press(p(1000, 500));
        assert_eq!(moved(&mut gesture, p(1010, 500), &zones), None);
        assert_eq!(
            moved(&mut gesture, p(1450, 500), &zones),
            Some(Signal::Arm {
                zones: 0b01,
                generation: 7
            })
        );
        // A bogus negative threshold counts as none.
        gesture.release();
        gesture.press(p(1450, 500));
        assert!(gesture.moved(p(1451, 500), (-3, -3), &zones, 1.0).is_some());
    }

    #[test]
    fn a_missed_release_is_made_up_for_at_the_next_press() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        gesture.press(p(1000, 500));
        assert!(moved(&mut gesture, p(1500, 500), &zones).is_some());
        // The button-up never reached the hook; the next press disarms.
        assert_eq!(
            gesture.press(p(1000, 500)),
            Some(Signal::Disarm { generation: 7 })
        );
        // And the new gesture arms afresh.
        assert_eq!(
            moved(&mut gesture, p(1500, 500), &zones),
            Some(Signal::Arm {
                zones: 0b01,
                generation: 7
            })
        );
    }

    #[test]
    fn zones_replaced_mid_drag_are_armed_afresh() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        gesture.press(p(1000, 500));
        assert!(moved(&mut gesture, p(1500, 500), &zones).is_some());
        let replaced = ZoneSet {
            generation: 8,
            ..zones
        };
        assert_eq!(
            moved(&mut gesture, p(1501, 500), &replaced),
            Some(Signal::Arm {
                zones: 0b01,
                generation: 8
            })
        );
        assert_eq!(gesture.release(), Some(Signal::Disarm { generation: 8 }));
        // No zones at all: a drag arms nothing, and its release says so.
        gesture.press(p(1000, 500));
        assert_eq!(moved(&mut gesture, p(1500, 500), &ZoneSet::EMPTY), None);
        assert_eq!(gesture.release(), None);
    }

    #[test]
    fn files_dragged_over_an_armed_window_light_it_up() {
        let mut target = Target::default();
        assert_eq!(target.look(), Look::Hidden);
        target.arm();
        assert_eq!(target.look(), Look::Armed);
        assert!(target.look().shown());
        assert_eq!(target.drag_enter(true, true), Effect::Copy);
        assert_eq!(target.look(), Look::Highlighted);
        assert_eq!(target.drag_over(true), Effect::Copy);
        // Leaving goes back to invisible but there.
        target.drag_leave();
        assert_eq!(target.look(), Look::Armed);
        assert_eq!(target.drag_over(true), Effect::None);
    }

    #[test]
    fn only_files_and_only_copies_are_taken() {
        let mut target = Target::default();
        target.arm();
        // Text or a link dragged over it: it stays invisible and refuses.
        assert_eq!(target.drag_enter(false, true), Effect::None);
        assert_eq!(target.look(), Look::Armed);
        assert_eq!(target.drag_over(true), Effect::None);
        // Files from a source that only allows moving them: shown, refused.
        assert_eq!(target.drag_enter(true, false), Effect::None);
        assert_eq!(target.look(), Look::Highlighted);
        assert_eq!(target.drag_over(false), Effect::None);
        // The source allows a copy again (the user let go of Shift).
        assert_eq!(target.drag_over(true), Effect::Copy);
        // A drop the source does not allow to copy is refused, and hides.
        assert_eq!(target.drop_files(true, false), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn a_drop_copies_and_hides() {
        let mut target = Target::default();
        target.arm();
        target.drag_enter(true, true);
        assert_eq!(target.drop_files(true, true), Effect::Copy);
        assert_eq!(target, Target::default());
        // A drop that gave no paths is refused.
        target.arm();
        target.drag_enter(true, true);
        assert_eq!(target.drop_files(false, true), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
        // A drop that comes after the window hid is still taken.
        assert_eq!(target.drop_files(true, true), Effect::Copy);
    }

    #[test]
    fn a_hidden_window_takes_no_drag() {
        let mut target = Target::default();
        assert_eq!(target.drag_enter(true, true), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
        assert_eq!(target.drag_over(true), Effect::None);
        target.drag_leave();
        assert_eq!(target.look(), Look::Hidden);
        target.disarm(Instant::now());
        target.tick(Instant::now() + Duration::from_secs(60), false);
        assert_eq!(target, Target::default());
    }

    #[test]
    fn the_window_hides_soon_after_the_button_goes_up() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t);
        target.tick(t + HIDE_DELAY - Duration::from_millis(1), false);
        assert_eq!(target.look(), Look::Armed);
        target.tick(t + HIDE_DELAY, false);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn a_new_drag_keeps_it() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t);
        // Pressed again and dragged near within the delay.
        target.arm();
        target.tick(t + HIDE_DELAY * 4, true);
        assert_eq!(target.look(), Look::Armed);
    }

    #[test]
    fn a_drop_on_its_way_after_the_button_up_keeps_it() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.drag_enter(true, true);
        // The hook's disarm can come before the drag source's drop.
        target.disarm(t);
        target.tick(t + HIDE_DELAY, false);
        assert_eq!(target.look(), Look::Highlighted);
        assert_eq!(target.drop_files(true, true), Effect::Copy);
        assert_eq!(target.look(), Look::Hidden);
        // A disarm after the drop changes nothing.
        target.disarm(t + HIDE_DELAY);
        assert_eq!(target, Target::default());
    }

    #[test]
    fn files_that_enter_after_the_button_up_still_find_it() {
        // The hook sees the button go up before the drag source has handled
        // its last move into the window.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t);
        assert_eq!(target.drag_enter(true, true), Effect::Copy);
        // The drop follows within the grace.
        target.tick(t + HIDE_DELAY, false);
        target.tick(t + HIDE_DELAY * 2, false);
        assert_eq!(target.look(), Look::Highlighted);
        assert_eq!(target.drop_files(true, true), Effect::Copy);
    }

    #[test]
    fn a_drop_that_never_comes_does_not_keep_it_forever() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.drag_enter(true, true);
        target.disarm(t);
        target.tick(
            t + HIDE_DELAY + DROP_GRACE - Duration::from_millis(1),
            false,
        );
        assert_eq!(target.look(), Look::Highlighted);
        target.tick(t + HIDE_DELAY + DROP_GRACE, false);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn a_cancelled_drag_hides_on_the_usual_delay() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.drag_enter(true, true);
        target.disarm(t);
        // Escape: the drag source says the files left.
        target.drag_leave();
        target.tick(t + HIDE_DELAY, false);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn a_missed_button_up_still_hides_it() {
        // Regression guard: an armed window is hit-tested, so one left
        // behind would swallow clicks at the edge.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        // The drag goes on as long as a button is held.
        target.tick(t, true);
        target.tick(t + Duration::from_secs(30), true);
        assert_eq!(target.look(), Look::Armed);
        // The button is up but no disarm came.
        let up = t + Duration::from_secs(31);
        target.tick(up, false);
        assert_eq!(target.look(), Look::Armed);
        target.tick(up + HIDE_DELAY, false);
        assert_eq!(target.look(), Look::Hidden);
        // Highlighted, it waits for the drop's grace too.
        target.arm();
        target.drag_enter(true, true);
        target.tick(up, false);
        target.tick(up + HIDE_DELAY + DROP_GRACE, false);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn a_click_on_a_window_left_after_a_drag_hides_it_at_once() {
        // Regression guard: the window lingers after the button-up, and a
        // click at the edge in that time lands on it.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t);
        target.clicked();
        assert_eq!(target, Target::default());
        // Files over it: the drop is on its way, so it stays.
        target.arm();
        target.drag_enter(true, true);
        target.clicked();
        assert_eq!(target.look(), Look::Highlighted);
        // Nothing shown, nothing to do.
        let mut hidden = Target::default();
        hidden.clicked();
        assert_eq!(hidden, Target::default());
    }

    #[test]
    fn the_first_disarm_sets_the_time() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t);
        target.disarm(t + Duration::from_millis(400));
        target.tick(t + HIDE_DELAY, false);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn drops_wait_in_order_and_the_oldest_go_first() {
        let mut waiting = VecDeque::new();
        queue_drop(&mut waiting, Vec::new());
        assert!(waiting.is_empty());
        for n in 0..MAX_WAITING_DROPS + 3 {
            queue_drop(&mut waiting, vec![PathBuf::from(format!("file {n}"))]);
        }
        assert_eq!(waiting.len(), MAX_WAITING_DROPS);
        assert_eq!(waiting.front(), Some(&vec![PathBuf::from("file 3")]));
        assert_eq!(
            waiting.back(),
            Some(&vec![PathBuf::from(format!(
                "file {}",
                MAX_WAITING_DROPS + 2
            ))])
        );
    }
}
