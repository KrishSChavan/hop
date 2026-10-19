//! The drop zone's rules (milestone M5): where the windows along the edges
//! that lead to the other computer go, when a drag arms them, and what they
//! show and answer while files are dragged over them
//! (docs/M4-M5-DESIGN.md, section 5.1).
//!
//! # Where
//!
//! A zone is the part of one display's edge that leads to the other
//! computer (`hop_platform::DropZone`, in logical units). It has two windows
//! along that part of the edge, inside the display ([`Placement`], in
//! physical pixels, converted the way the rest of the backend converts):
//!
//! - **The band**, [`BAND_DEPTH`] deep: the only one that takes drops, and
//!   the only one a click can land on. It covers no more than the very
//!   edge, where a cursor pushed against it stops, so the icons, panes and
//!   scroll bars near the edge keep their own drops and clicks. While files
//!   are over it, it grows to the label's place, under the label
//!   ([`Placement::rect`]): a drop anywhere on the box the user sees is
//!   taken, and the box goes only when the files leave it.
//! - **The label**, [`LABEL_DEPTH`] deep: the tint and the words that say
//!   what a drop there does. Drags and clicks pass through it, to the band
//!   under it, and it shows only while files are over the band.
//!
//! # When
//!
//! The band shows only during a drag near its zone, so it never gets in the
//! way of ordinary clicks or of window snapping at the edge. The mouse hook
//! follows each left-button gesture ([`DragGesture`]): once the cursor has
//! moved past the system's drag threshold from where the button went down,
//! every zone within [`ARM_REACH`] of the cursor is armed, and the button
//! going up disarms them. The hook sees Hop's own injected buttons and moves
//! too, so a drag the other computer drives here arms the zones as well.
//! The reach is measured in logical units, like the zones, so it is more
//! than the band's depth on every display scale: the band is there before
//! the cursor reaches it.
//!
//! # What it shows ([`Target`])
//!
//! - **Hidden.**
//! - **Armed:** the band shows at [`BAND_ALPHA`]: invisible, but there for
//!   drag and drop to find under the cursor.
//! - **Highlighted:** files are dragged over the band: the band grows to
//!   the label's place, still invisible, and the label shows over it, at
//!   [`LABEL_ALPHA`]. The band answers "copy" if the source allows it. The
//!   cursor that brought the files is still on the band once it has grown,
//!   so drag and drop sees no change; it tells the band the files left only
//!   once they leave the whole box.
//!
//! Anything but files (text, a link, an e-mail, an attachment that is not a
//! file yet) dragged onto an armed band makes it step aside: it hides at
//! once and stays hidden until that gesture ends, so drag and drop finds
//! the window under it, which gets the drag and its drop as if Hop were not
//! there. The files leaving the box (or the drag being cancelled) go back
//! to armed: the label goes, and the band shrinks back to the edge. A drop
//! takes the files, answers "copy" and hides. The answer is never "move":
//! the files always stay where they were.
//!
//! When the button goes up somewhere else, an armed band hides at once: no
//! drop can come to it, and the next click must not find it. When it goes
//! up over the band (or the hook missed where), the drop it starts may
//! still be on its way, so the band hides [`HIDE_DELAY`] later, or
//! [`DROP_GRACE`] after that while files are over it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hop_platform::DropZone;
use hop_types::{Edge, Point};

use super::coords::{PhysPoint, PhysRect, coord_to_physical, to_logical};

/// How deep the band reaches into the display, in logical units (8 pixels
/// at 100% scaling, 10 at 125%): enough to hold the display's outermost
/// line of pixels, where a cursor pushed against the edge stops, and little
/// more, so what lies near the edge keeps its own drops and clicks.
pub const BAND_DEPTH: f64 = 8.0;

/// How deep the label reaches into the display, in logical units.
pub const LABEL_DEPTH: f64 = 120.0;

/// How near a drag must come to a zone to arm it, in logical units (150
/// pixels at 100% scaling). Far more than [`BAND_DEPTH`], so the band is
/// there before the cursor reaches it.
pub const ARM_REACH: f64 = 150.0;

/// How long a band stays after the button goes up over it, so that the
/// drop the button-up starts, and the drag source's last moves, still find
/// it: the hook sees the button go up before the drag source does.
pub const HIDE_DELAY: Duration = Duration::from_millis(500);

/// How much longer a highlighted band waits for its drop after
/// [`HIDE_DELAY`], before it hides anyway: the drag source drops at once, so
/// this only covers one that never finishes.
pub const DROP_GRACE: Duration = Duration::from_secs(5);

/// How often the drop thread checks, while a band shows (or stands aside),
/// whether it is due to hide, in milliseconds.
pub const TICK_MS: u32 = 100;

/// The most zones there can be: an arm names its zones with one bit each.
/// Zones past this are left out (a layout has a handful).
pub const MAX_ZONES: usize = 64;

/// The band's alpha: invisible, but unlike alpha 0 still hit-tested, so
/// drag and drop finds it.
pub const BAND_ALPHA: u8 = 1;

/// The label's alpha, of 255.
pub const LABEL_ALPHA: u8 = 200;

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

/// Where a window along `zone`, reaching `depth` logical units into its
/// display, goes, in physical pixels, with `scale` physical pixels per
/// logical unit (the primary display's scale), or `None` if it covers no
/// pixel.
///
/// Logical coordinates become pixels the way the rest of the backend turns
/// them (`coords::coord_to_physical`), so a window along a display's edge
/// ends exactly at that display's last pixel. A display narrower (or
/// shorter) than `depth` is covered whole.
pub fn window_rect(zone: &DropZone, scale: f64, depth: f64) -> Option<PhysRect> {
    let line = EdgeLine::of(zone)?;
    let display = zone.display;
    let px = |v: f64| coord_to_physical(v, scale);
    let (first, past) = (px(line.from), px(line.to));
    let width = depth.min(display.size.width);
    let height = depth.min(display.size.height);
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

/// Where a zone's two windows go, in physical pixels (see [`window_rect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The band at the very edge, [`BAND_DEPTH`] deep.
    pub band: PhysRect,
    /// The label, [`LABEL_DEPTH`] deep: over the band and beyond it.
    pub label: PhysRect,
}

impl Placement {
    /// Where the windows of `zone` go at `scale`, or `None` if they would
    /// cover no pixel.
    pub fn of(zone: &DropZone, scale: f64) -> Option<Placement> {
        Some(Placement {
            band: window_rect(zone, scale, BAND_DEPTH)?,
            label: window_rect(zone, scale, LABEL_DEPTH)?,
        })
    }

    /// Where `part` goes while the zone looks `look`. The label has one
    /// place. The band stays at the very edge, except while files are over
    /// it: then it covers the label's place, under the label, so that a
    /// drop anywhere on the box the user sees is the band's, and drag and
    /// drop tells the band the files left only once they leave that box.
    pub fn rect(&self, part: Part, look: Look) -> PhysRect {
        match part {
            Part::Band if !look.labelled() => self.band,
            Part::Band | Part::Label => self.label,
        }
    }
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
    /// Show these zones' bands (one bit each), armed.
    Arm {
        /// Every zone this gesture armed so far.
        zones: u64,
        /// The [`ZoneSet::generation`] they belong to.
        generation: u64,
    },
    /// The gesture ended: hide the bands, at once or soon (see
    /// [`Target::disarm`]).
    Disarm {
        /// The [`ZoneSet::generation`] the armed bands belong to.
        generation: u64,
        /// Where the left button went up (physical pixels), or `None` if
        /// the hook did not see it go up (it missed the button-up, or
        /// capture stopped mid-drag).
        at: Option<PhysPoint>,
    },
}

/// A point as one number, for a window message: x in the high 32 bits, y
/// in the low 32.
pub fn pack_point(p: PhysPoint) -> u64 {
    (u64::from(p.x.cast_unsigned()) << 32) | u64::from(p.y.cast_unsigned())
}

/// The point [`pack_point`] packed.
pub fn unpack_point(packed: u64) -> PhysPoint {
    // Each half holds one coordinate's bits.
    PhysPoint::new(
        ((packed >> 32) as u32).cast_signed(),
        (packed as u32).cast_signed(),
    )
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
        let stale = self.end(None);
        self.press = Some(at);
        stale
    }

    /// The left button went up at `at`. Disarms the zones this gesture
    /// armed.
    pub fn release(&mut self, at: PhysPoint) -> Option<Signal> {
        self.end(Some(at))
    }

    /// The gesture ends without the hook seeing the button go up (capture
    /// stopped). Disarms the zones this gesture armed.
    pub fn forget(&mut self) -> Option<Signal> {
        self.end(None)
    }

    fn end(&mut self, at: Option<PhysPoint>) -> Option<Signal> {
        let signal = (self.armed != 0).then_some(Signal::Disarm {
            generation: self.generation,
            at,
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

/// What a zone's windows show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Look {
    /// Neither window is shown.
    #[default]
    Hidden,
    /// The band shows at the very edge, at [`BAND_ALPHA`]: invisible, but
    /// there for drag and drop.
    Armed,
    /// Files are dragged over the band: it covers the label's place, and
    /// the label shows over it.
    Highlighted,
}

impl Look {
    /// Whether the band is on screen, visible or not.
    pub fn shown(self) -> bool {
        self != Look::Hidden
    }

    /// Whether the label is on screen.
    pub fn labelled(self) -> bool {
        self == Look::Highlighted
    }
}

/// One of a zone's two windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The band, which takes drops.
    Band,
    /// The label, which drags and clicks pass through.
    Label,
}

/// What a zone's windows do for its look to go from `before` to `after`:
/// each window that changes, with whether it ends up shown, in order; it
/// goes where [`Placement::rect`] says for `after`. The band changes when
/// it comes or goes, and also when it grows to the label's place or
/// shrinks back to the edge. The label goes first and comes last, so it is
/// never on screen without the band under all of it, which takes the drop
/// it invites; and each window is put on top as it is shown, so the label,
/// shown after the band, stays over it.
pub fn window_steps(before: Look, after: Look) -> impl Iterator<Item = (Part, bool)> {
    let label_goes = before.labelled() && !after.labelled();
    let band_changes = before.shown() != after.shown() || before.labelled() != after.labelled();
    let label_comes = !before.labelled() && after.labelled();
    [
        label_goes.then_some((Part::Label, false)),
        band_changes.then_some((Part::Band, after.shown())),
        label_comes.then_some((Part::Label, true)),
    ]
    .into_iter()
    .flatten()
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

/// Where the left button went up, as it matters to one band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonUp {
    /// Over the band, or the hook did not see where: the drop the button-up
    /// starts may still be on its way to the band.
    Over,
    /// Somewhere else: no drop can come to the band.
    Elsewhere,
}

impl ButtonUp {
    /// Where a button-up at `at` (`None`: not seen) is for a band at
    /// `band`. Drag and drop drops where the button went up, so a drop can
    /// come to the band only from a button-up over it. Only an armed band
    /// is decided this way (one with files over it waits for its drop
    /// wherever the button went up), so `band` is its place at the very
    /// edge, [`Placement::band`].
    pub fn locate(at: Option<PhysPoint>, band: PhysRect) -> ButtonUp {
        if at.is_none_or(|at| band.contains(at)) {
            ButtonUp::Over
        } else {
            ButtonUp::Elsewhere
        }
    }
}

/// One zone's state (see the module documentation). The calls follow what
/// drag and drop tells the band (`IDropTarget`), plus the hook's arm and
/// disarm and the drop thread's timer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    look: Look,
    /// When to hide, once the button went up.
    hide_at: Option<Instant>,
    /// Something other than files was dragged onto the band: it stepped
    /// aside, and no arm shows it again until the gesture ends.
    stepped_aside: bool,
}

impl Target {
    /// What the windows show now.
    pub fn look(&self) -> Look {
        self.look
    }

    /// Whether the drop thread's timer has work for this zone: the band
    /// shows, or it stepped aside until the gesture ends (see
    /// [`Target::tick`]).
    pub fn busy(&self) -> bool {
        self.look.shown() || self.stepped_aside
    }

    /// A drag came within reach: show the band, invisibly, for drag and
    /// drop to find. The drag goes on, so a pending hide is cancelled. A
    /// band that stepped aside during this gesture stays hidden.
    pub fn arm(&mut self) {
        if self.stepped_aside {
            return;
        }
        if self.look == Look::Hidden {
            self.look = Look::Armed;
        }
        self.hide_at = None;
    }

    /// The gesture ended at `now`, with the left button going up `up` (see
    /// [`ButtonUp`]). A band that stepped aside may show again at the next
    /// arm. A band with files over it, or one the button went up over,
    /// hides [`HIDE_DELAY`] later (see [`Target::tick`]): their drop may
    /// still be on its way. Any other hides at once: no drop can come to
    /// it, so nothing is left at the edge to catch the next click.
    pub fn disarm(&mut self, now: Instant, up: ButtonUp) {
        self.stepped_aside = false;
        match self.look {
            Look::Hidden => {}
            Look::Armed if up == ButtonUp::Elsewhere => *self = Target::default(),
            Look::Armed | Look::Highlighted => {
                self.hide_at.get_or_insert(now + HIDE_DELAY);
            }
        }
    }

    /// Something is dragged onto the band. `files`: the data holds file
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
            // Not files: step aside until the gesture ends. Hidden, the
            // band is no longer under the cursor, so drag and drop finds
            // the window under it, which gets the drag and its drop.
            _ => {
                *self = Target {
                    stepped_aside: true,
                    ..Target::default()
                };
                Effect::None
            }
        }
    }

    /// The drag moves over the band: the answer stays the one it got on
    /// entering, for what the source allows now.
    pub fn drag_over(&self, copy_allowed: bool) -> Effect {
        if self.look == Look::Highlighted {
            answer(copy_allowed)
        } else {
            Effect::None
        }
    }

    /// The drag left the band, or was cancelled: the label goes, and the
    /// band shrinks back to the edge.
    pub fn drag_leave(&mut self) {
        if self.look == Look::Highlighted {
            self.look = Look::Armed;
        }
    }

    /// A click landed on the band, still shown after a drag: no drag is
    /// going on (a drag holds the mouse until its button goes up, which is
    /// when drag and drop hands over its drop), so it hides at once and the
    /// next click goes through. So does a highlighted band whose drop never
    /// came, which covers the whole label's place. A drop that comes after
    /// all is still taken (see [`Target::drop_files`]).
    pub fn clicked(&mut self) {
        *self = Target::default();
    }

    /// The drag was dropped on the band. `files`: paths were read from it.
    /// The windows hide, whatever the answer; a drop that comes after the
    /// band hid is still taken, since the user dropped it here.
    pub fn drop_files(&mut self, files: bool, copy_allowed: bool) -> Effect {
        *self = Target::default();
        if files {
            answer(copy_allowed)
        } else {
            Effect::None
        }
    }

    /// The drop thread's timer, every [`TICK_MS`]: hides the windows when
    /// they are due. `button_down`: a mouse button is held now, so a drag
    /// may go on. A shown band with no button held hides as if the
    /// button-up had been seen, so a missed one (the hooks were removed,
    /// capture stopped) cannot leave it there; and with no button held, the
    /// gesture a band stepped aside for is over, so a missed disarm cannot
    /// keep it hidden for good.
    pub fn tick(&mut self, now: Instant, button_down: bool) {
        if !button_down {
            self.stepped_aside = false;
        }
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
        // 8 units are 10 pixels, 120 are 150; 10 and 854 units are rows 12
        // and 1067.
        assert_eq!(
            Placement::of(&left, 1.25),
            Some(Placement {
                band: PhysRect::new(0, 12, 10, 1067),
                label: PhysRect::new(0, 12, 150, 1067),
            })
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
            Placement::of(&right, 1.25),
            Some(Placement {
                band: PhysRect::new(1910, 0, 1920, 1080),
                label: PhysRect::new(1770, 0, 1920, 1080),
            })
        );
        assert_eq!(EdgeLine::of(&right).map(|line| line.at), Some(1536.0));
        // A display right of the primary, at 100%.
        let second = Rect::new(1536.0, -100.0, 2560.0, 1440.0);
        let zone = zone(second, Edge::Right, -100.0, 1340.0);
        assert_eq!(
            Placement::of(&zone, 1.0),
            Some(Placement {
                band: PhysRect::new(4088, -100, 4096, 1340),
                label: PhysRect::new(3976, -100, 4096, 1340),
            })
        );
    }

    #[test]
    fn top_and_bottom_zones_run_along_the_rows() {
        let top = zone(panel(), Edge::Top, 100.0, 300.0);
        assert_eq!(
            Placement::of(&top, 1.0),
            Some(Placement {
                band: PhysRect::new(100, 0, 300, 8),
                label: PhysRect::new(100, 0, 300, 120),
            })
        );
        let bottom = zone(panel(), Edge::Bottom, 100.0, 300.0);
        assert_eq!(
            Placement::of(&bottom, 1.0),
            Some(Placement {
                band: PhysRect::new(100, 856, 300, 864),
                label: PhysRect::new(100, 744, 300, 864),
            })
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
            Placement::of(&bottom, 1.5),
            Some(Placement {
                band: PhysRect::new(150, 1284, 450, 1296),
                label: PhysRect::new(150, 1116, 450, 1296),
            })
        );
    }

    #[test]
    fn the_band_holds_the_displays_outermost_pixels_at_every_scale() {
        // Regression guard: the band takes drops only where a cursor pushed
        // against the edge stops, on the display's outermost line of pixels,
        // and reaches hardly further in.
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 3.0, 3.5] {
            // A 1920x1080 panel in logical units, as the backend reports it.
            let size = to_logical(PhysPoint::new(1920, 1080), scale);
            let display = Rect::new(0.0, 0.0, size.x, size.y);
            for (edge, outermost) in [
                (Edge::Left, p(0, 540)),
                (Edge::Right, p(1919, 540)),
                (Edge::Top, p(960, 0)),
                (Edge::Bottom, p(960, 1079)),
            ] {
                let (start, end) = display.edge_span(edge);
                let band = window_rect(&zone(display, edge, start, end), scale, BAND_DEPTH)
                    .expect("a band");
                assert!(band.contains(outermost), "{edge:?} at {scale}: {band:?}");
                let depth = if edge.is_vertical() {
                    band.width()
                } else {
                    band.height()
                };
                assert!(
                    depth >= 8 && f64::from(depth) <= BAND_DEPTH * scale + 1.0,
                    "{edge:?} at {scale}: {depth} pixels"
                );
            }
        }
    }

    #[test]
    fn the_label_lies_over_the_band_and_beyond() {
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let (start, end) = panel().edge_span(edge);
            let placement = Placement::of(&zone(panel(), edge, start, end), 1.25).expect("a place");
            let (band, label) = (placement.band, placement.label);
            assert!(
                label.left <= band.left
                    && label.top <= band.top
                    && label.right >= band.right
                    && label.bottom >= band.bottom,
                "{edge:?}: {placement:?}"
            );
            assert!(label.width() * label.height() > band.width() * band.height());
        }
    }

    #[test]
    fn a_zone_never_reaches_past_its_display() {
        // A display narrower than the label: the label covers all of it,
        // the band only its edge.
        let narrow = Rect::new(0.0, 0.0, 80.0, 600.0);
        assert_eq!(
            Placement::of(&zone(narrow, Edge::Right, 0.0, 600.0), 1.0),
            Some(Placement {
                band: PhysRect::new(72, 0, 80, 600),
                label: PhysRect::new(0, 0, 80, 600),
            })
        );
        // Narrower than the band too: both cover all of it.
        let sliver = Rect::new(0.0, 0.0, 5.0, 600.0);
        assert_eq!(
            Placement::of(&zone(sliver, Edge::Left, 0.0, 600.0), 1.0),
            Some(Placement {
                band: PhysRect::new(0, 0, 5, 600),
                label: PhysRect::new(0, 0, 5, 600),
            })
        );
        // A zone running past the display's ends is cut to the display.
        let long = zone(panel(), Edge::Left, -500.0, 5000.0);
        assert_eq!(
            Placement::of(&long, 1.0),
            Some(Placement {
                band: PhysRect::new(0, 0, 8, 864),
                label: PhysRect::new(0, 0, 120, 864),
            })
        );
        assert_eq!(
            EdgeLine::of(&long).map(|line| (line.from, line.to)),
            Some((0.0, 864.0))
        );
    }

    #[test]
    fn empty_or_nonsense_zones_have_no_place() {
        let none = |zone: DropZone| {
            assert_eq!(Placement::of(&zone, 1.0), None, "{zone:?}");
            assert_eq!(window_rect(&zone, 1.0, BAND_DEPTH), None, "{zone:?}");
            assert_eq!(EdgeLine::of(&zone), None, "{zone:?}");
        };
        none(zone(panel(), Edge::Left, 300.0, 300.0));
        none(zone(panel(), Edge::Left, 300.0, 200.0));
        none(zone(panel(), Edge::Left, f64::NAN, 200.0));
        none(zone(panel(), Edge::Top, 2000.0, 3000.0));
        // Less than a pixel has an edge, but no window.
        let sliver = zone(panel(), Edge::Left, 10.2, 10.6);
        assert!(EdgeLine::of(&sliver).is_some());
        assert_eq!(Placement::of(&sliver, 1.0), None);
        let flat = Rect::new(0.0, 0.0, 0.0, 600.0);
        assert_eq!(
            Placement::of(&zone(flat, Edge::Right, 0.0, 600.0), 1.0),
            None
        );
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
    fn the_reach_is_more_than_the_bands_depth_at_every_scale() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            assert!(
                coord_to_physical(ARM_REACH, scale) > coord_to_physical(BAND_DEPTH, scale),
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
        assert_eq!(gesture.release(p(1526, 496)), None);
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
        // The release says where the button went up.
        assert_eq!(
            gesture.release(p(250, 20)),
            Some(Signal::Disarm {
                generation: 7,
                at: Some(p(250, 20))
            })
        );
        assert_eq!(gesture.release(p(250, 20)), None);
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
        gesture.release(p(1450, 505));
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
        gesture.release(p(1450, 500));
        gesture.press(p(1450, 500));
        assert!(gesture.moved(p(1451, 500), (-3, -3), &zones, 1.0).is_some());
    }

    #[test]
    fn a_missed_release_is_made_up_for_at_the_next_press() {
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        gesture.press(p(1000, 500));
        assert!(moved(&mut gesture, p(1500, 500), &zones).is_some());
        // The button-up never reached the hook; the next press disarms,
        // without a place: where the button went up is not known.
        assert_eq!(
            gesture.press(p(1000, 500)),
            Some(Signal::Disarm {
                generation: 7,
                at: None
            })
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
    fn a_forgotten_gesture_disarms_without_a_place() {
        // Capture stopped mid-drag: the hook will not see the button go up.
        let zones = right_and_top();
        let mut gesture = DragGesture::default();
        gesture.press(p(1000, 500));
        assert!(moved(&mut gesture, p(1500, 500), &zones).is_some());
        assert_eq!(
            gesture.forget(),
            Some(Signal::Disarm {
                generation: 7,
                at: None
            })
        );
        assert!(!gesture.held());
        // A gesture that armed nothing has nothing to disarm.
        assert_eq!(gesture.forget(), None);
        gesture.press(p(1000, 500));
        assert_eq!(gesture.forget(), None);
    }

    #[test]
    fn a_point_survives_the_window_message() {
        for at in [
            p(0, 0),
            p(1919, 1079),
            p(-1, -1),
            p(-2560, 1440),
            p(3839, -1080),
            p(i32::MIN, i32::MAX),
            p(i32::MAX, i32::MIN),
        ] {
            assert_eq!(unpack_point(pack_point(at)), at, "{at:?}");
        }
        // x is in the high half, y in the low one.
        assert_eq!(pack_point(p(1, 2)), (1 << 32) | 2);
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
        assert_eq!(
            gesture.release(p(1501, 500)),
            Some(Signal::Disarm {
                generation: 8,
                at: Some(p(1501, 500))
            })
        );
        // No zones at all: a drag arms nothing, and its release says so.
        gesture.press(p(1000, 500));
        assert_eq!(moved(&mut gesture, p(1500, 500), &ZoneSet::EMPTY), None);
        assert_eq!(gesture.release(p(1500, 500)), None);
    }

    #[test]
    fn files_dragged_over_an_armed_band_light_it_up() {
        let mut target = Target::default();
        assert_eq!(target.look(), Look::Hidden);
        assert!(!target.look().shown() && !target.look().labelled());
        // Armed: the band only, invisible.
        target.arm();
        assert_eq!(target.look(), Look::Armed);
        assert!(target.look().shown() && !target.look().labelled());
        // Files over the band: the label shows too.
        assert_eq!(target.drag_enter(true, true), Effect::Copy);
        assert_eq!(target.look(), Look::Highlighted);
        assert!(target.look().shown() && target.look().labelled());
        assert_eq!(target.drag_over(true), Effect::Copy);
        // Leaving goes back to the band alone, invisible but there.
        target.drag_leave();
        assert_eq!(target.look(), Look::Armed);
        assert_eq!(target.drag_over(true), Effect::None);
    }

    #[test]
    fn the_label_shows_only_over_a_band_with_files() {
        let steps = |before, after| window_steps(before, after).collect::<Vec<_>>();
        use Look::{Armed, Hidden, Highlighted};
        assert_eq!(steps(Hidden, Armed), [(Part::Band, true)]);
        // Files over the band: it grows to the label's place first, then
        // the label shows over it.
        assert_eq!(
            steps(Armed, Highlighted),
            [(Part::Band, true), (Part::Label, true)]
        );
        // They leave the box: the label goes first, then the band shrinks
        // back to the edge.
        assert_eq!(
            steps(Highlighted, Armed),
            [(Part::Label, false), (Part::Band, true)]
        );
        assert_eq!(steps(Armed, Hidden), [(Part::Band, false)]);
        // A drop: the label goes first.
        assert_eq!(
            steps(Highlighted, Hidden),
            [(Part::Label, false), (Part::Band, false)]
        );
        // Never happens, but the band would come first.
        assert_eq!(
            steps(Hidden, Highlighted),
            [(Part::Band, true), (Part::Label, true)]
        );
        // Nothing changes, nothing moves.
        for look in [Hidden, Armed, Highlighted] {
            assert_eq!(steps(look, look), [], "{look:?}");
        }
    }

    #[test]
    fn the_box_the_user_sees_is_all_band_and_the_label_stays_on_top() {
        // Regression guard: with the band left at the edge under a deep
        // label, a drop on the label's words went to the window under it,
        // and moving onto them made the box vanish.
        let placement =
            Placement::of(&zone(panel(), Edge::Left, 10.0, 854.0), 1.25).expect("a place");
        let looks = [Look::Hidden, Look::Armed, Look::Highlighted];
        for before in looks {
            for after in looks {
                // Where each window is on screen, and whether the label was
                // last put on top of the band.
                let mut band = before.shown().then(|| placement.rect(Part::Band, before));
                let mut label = before
                    .labelled()
                    .then(|| placement.rect(Part::Label, before));
                let mut label_on_top = true;
                for (part, shown) in window_steps(before, after) {
                    let rect = shown.then(|| placement.rect(part, after));
                    match part {
                        Part::Band => band = rect,
                        Part::Label => label = rect,
                    }
                    if shown {
                        // Each window goes on top as it is shown or moved.
                        label_on_top = part == Part::Label;
                    }
                    if label.is_some() {
                        // The label is never on screen without the band
                        // under all of it, which takes the drop it invites.
                        assert_eq!(band, label, "{before:?} to {after:?}");
                        assert!(label_on_top, "{before:?} to {after:?}");
                    }
                }
                // Every change ends with the windows where the look says.
                assert_eq!(
                    band,
                    after.shown().then(|| placement.rect(Part::Band, after)),
                    "{before:?} to {after:?}"
                );
                assert_eq!(
                    label,
                    after.labelled().then(|| placement.rect(Part::Label, after)),
                    "{before:?} to {after:?}"
                );
            }
        }
    }

    #[test]
    fn a_highlighted_band_covers_the_labels_place() {
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let (start, end) = panel().edge_span(edge);
            let placement = Placement::of(&zone(panel(), edge, start, end), 1.25).expect("a place");
            let band = |look| placement.rect(Part::Band, look);
            // Armed, the band is the thin strip at the edge...
            assert_eq!(band(Look::Armed), placement.band, "{edge:?}");
            assert_eq!(band(Look::Hidden), placement.band, "{edge:?}");
            // ...and with files over it, exactly the label's place, so a
            // drop on the label's words, in its middle, is the band's.
            assert_eq!(band(Look::Highlighted), placement.label, "{edge:?}");
            assert_eq!(
                placement.rect(Part::Label, Look::Highlighted),
                placement.label
            );
            let words = placement.label.center();
            assert!(band(Look::Highlighted).contains(words), "{edge:?}");
            assert!(!band(Look::Armed).contains(words), "{edge:?}");
            // The cursor that brought the files onto the thin band is still
            // on it once it has grown: drag and drop sees no files leave.
            let thin = placement.band;
            for corner in [
                p(thin.left, thin.top),
                p(thin.right - 1, thin.top),
                p(thin.left, thin.bottom - 1),
                p(thin.right - 1, thin.bottom - 1),
            ] {
                assert!(band(Look::Highlighted).contains(corner), "{edge:?}");
            }
        }
    }

    #[test]
    fn only_files_and_only_copies_are_taken() {
        let mut target = Target::default();
        target.arm();
        // Text or a link dragged over it: refused, and it steps aside.
        assert_eq!(target.drag_enter(false, true), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
        assert_eq!(target.drag_over(true), Effect::None);
        // The next drag: files from a source that only allows moving them:
        // shown, refused.
        target.disarm(Instant::now(), ButtonUp::Elsewhere);
        target.arm();
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
    fn anything_but_files_makes_it_step_aside_until_the_gesture_ends() {
        // Regression guard: text, a link or an e-mail dragged over an armed
        // band must reach the app under it, not meet an invisible band that
        // refuses it.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        assert_eq!(target.drag_enter(false, true), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
        // Drag and drop moves on to the window under it.
        target.drag_leave();
        assert_eq!(target.look(), Look::Hidden);
        // The same gesture arming again (it came near another zone) does
        // not bring it back, however long the drag goes on.
        target.arm();
        assert_eq!(target.look(), Look::Hidden);
        assert!(target.busy());
        target.tick(t + Duration::from_secs(30), true);
        target.arm();
        assert_eq!(target.look(), Look::Hidden);
        // The button goes up: the next gesture arms it again.
        target.disarm(t + Duration::from_secs(31), ButtonUp::Over);
        assert!(!target.busy());
        assert_eq!(target.look(), Look::Hidden);
        target.arm();
        assert_eq!(target.look(), Look::Armed);
    }

    #[test]
    fn a_missed_disarm_does_not_keep_it_aside_for_good() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.drag_enter(false, true);
        // The button-up never reached the hook, but the timer finds no
        // button held: that gesture is over.
        target.tick(t, false);
        assert!(!target.busy());
        target.arm();
        assert_eq!(target.look(), Look::Armed);
        // Text that enters only after the button went up over the band
        // makes it step aside too, until the timer sees the button up.
        target.disarm(t, ButtonUp::Over);
        assert_eq!(target.drag_enter(false, true), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
        target.tick(t, false);
        assert_eq!(target, Target::default());
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
        // A drop that comes after the band hid is still taken.
        assert_eq!(target.drop_files(true, true), Effect::Copy);
    }

    #[test]
    fn a_hidden_band_takes_no_drag() {
        let mut target = Target::default();
        assert_eq!(target.drag_enter(true, true), Effect::None);
        assert_eq!(target.look(), Look::Hidden);
        assert_eq!(target.drag_over(true), Effect::None);
        target.drag_leave();
        assert_eq!(target.look(), Look::Hidden);
        target.disarm(Instant::now(), ButtonUp::Over);
        target.tick(Instant::now() + Duration::from_secs(60), false);
        assert_eq!(target, Target::default());
    }

    #[test]
    fn a_drop_can_come_only_where_the_button_went_up() {
        // The left band at 125%.
        let band = PhysRect::new(0, 12, 10, 1067);
        let locate = |at| ButtonUp::locate(at, band);
        assert_eq!(locate(Some(p(0, 500))), ButtonUp::Over);
        assert_eq!(locate(Some(p(9, 12))), ButtonUp::Over);
        assert_eq!(locate(Some(p(10, 500))), ButtonUp::Elsewhere);
        assert_eq!(locate(Some(p(100, 500))), ButtonUp::Elsewhere);
        assert_eq!(locate(Some(p(0, 11))), ButtonUp::Elsewhere);
        assert_eq!(locate(Some(p(0, 1067))), ButtonUp::Elsewhere);
        assert_eq!(locate(Some(p(-1, 500))), ButtonUp::Elsewhere);
        // Not seen: a drop may still come.
        assert_eq!(locate(None), ButtonUp::Over);
    }

    #[test]
    fn the_band_hides_at_once_when_the_button_goes_up_elsewhere() {
        // Regression guard: after an ordinary drag near the edge (selecting
        // text, dragging a scroll bar), nothing invisible may stay at the
        // edge to catch the next click.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t, ButtonUp::Elsewhere);
        assert_eq!(target, Target::default());
        // Files that were over it and left, dropped somewhere else.
        target.arm();
        target.drag_enter(true, true);
        target.drag_leave();
        target.disarm(t, ButtonUp::Elsewhere);
        assert_eq!(target, Target::default());
    }

    #[test]
    fn the_band_hides_soon_after_the_button_goes_up_over_it() {
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t, ButtonUp::Over);
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
        target.disarm(t, ButtonUp::Over);
        // Pressed again and dragged near within the delay.
        target.arm();
        target.tick(t + HIDE_DELAY * 4, true);
        assert_eq!(target.look(), Look::Armed);
    }

    #[test]
    fn a_drop_on_its_way_after_the_button_up_keeps_it() {
        // Files over it wait for their drop wherever the hook saw the
        // button go up: the drag source decides, a moment later.
        for up in [ButtonUp::Over, ButtonUp::Elsewhere] {
            let t = Instant::now();
            let mut target = Target::default();
            target.arm();
            target.drag_enter(true, true);
            // The hook's disarm can come before the drag source's drop.
            target.disarm(t, up);
            target.tick(t + HIDE_DELAY, false);
            assert_eq!(target.look(), Look::Highlighted, "{up:?}");
            assert_eq!(target.drop_files(true, true), Effect::Copy);
            assert_eq!(target.look(), Look::Hidden);
            // A disarm after the drop changes nothing.
            target.disarm(t + HIDE_DELAY, up);
            assert_eq!(target, Target::default());
        }
    }

    #[test]
    fn files_that_enter_after_the_button_up_still_find_it() {
        // The hook sees the button go up over the band before the drag
        // source has handled its last move into it.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t, ButtonUp::Over);
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
        target.disarm(t, ButtonUp::Over);
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
        target.disarm(t, ButtonUp::Over);
        // Escape: the drag source says the files left.
        target.drag_leave();
        target.tick(t + HIDE_DELAY, false);
        assert_eq!(target.look(), Look::Hidden);
    }

    #[test]
    fn a_missed_button_up_still_hides_it() {
        // Regression guard: an armed band is hit-tested, so one left behind
        // would swallow clicks at the edge.
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
    fn a_click_on_a_band_left_after_a_drag_hides_it_at_once() {
        // Regression guard: the band lingers after the button goes up over
        // it, and a click at the edge in that time lands on it.
        let t = Instant::now();
        let mut target = Target::default();
        target.arm();
        target.disarm(t, ButtonUp::Over);
        target.clicked();
        assert_eq!(target, Target::default());
        // Files were over it and their drop never came (the drag source
        // went away): it covers the label's place, so it must not stay
        // there catching clicks until the grace runs out.
        target.arm();
        target.drag_enter(true, true);
        target.disarm(t, ButtonUp::Over);
        target.clicked();
        assert_eq!(target, Target::default());
        // A drop that comes after all is still taken.
        assert_eq!(target.drop_files(true, true), Effect::Copy);
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
        target.disarm(t, ButtonUp::Over);
        target.disarm(t + Duration::from_millis(400), ButtonUp::Over);
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
