//! Screen layout: where each device's displays sit in one shared plane, and
//! where the cursor lands when it leaves a display through an edge.
//!
//! Every device keeps its displays in its own logical coordinates. The layout
//! gives each device an `origin` in a shared global plane, so "the PC is to
//! the right of the Mac" becomes plain geometry. Lookups happen in the global
//! plane; the entry point they produce is converted back into the target
//! device's own coordinates.

use std::collections::{BTreeMap, BTreeSet};

use hop_types::{DeviceId, DisplayInfo, Edge, EntryPoint, Point, Rect};

/// Display coordinates and sizes beyond this magnitude are rejected. No real
/// arrangement comes close, and the bound keeps every later sum finite.
const MAX_COORDINATE: f64 = 1_000_000.0;

/// Why a layout or a display list was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// The same device was listed twice.
    #[error("device {0} is listed more than once")]
    DuplicateDevice(DeviceId),
    /// The device is not part of the layout.
    #[error("device {0} is not in the layout")]
    UnknownDevice(DeviceId),
    /// A device was asked to be its own neighbor.
    #[error("device {0} cannot be its own neighbor")]
    SelfNeighbor(DeviceId),
    /// Two displays of one device share an id.
    #[error("device {device} lists display {display} more than once")]
    DuplicateDisplay {
        /// The device that reported the displays.
        device: DeviceId,
        /// The repeated display id.
        display: u32,
    },
    /// A display has a non-finite, absurdly large or smaller-than-one-unit
    /// frame, or a scale that is not positive.
    #[error("device {device} reports display {display} with an unusable frame or scale")]
    InvalidDisplay {
        /// The device that reported the display.
        device: DeviceId,
        /// The display's id.
        display: u32,
    },
    /// A device origin is not a finite point of reasonable size.
    #[error("device {0} has an unusable origin")]
    InvalidOrigin(DeviceId),
}

/// One device's displays, placed in the shared plane.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceLayout {
    id: DeviceId,
    origin: Point,
    displays: Vec<DisplayInfo>,
}

impl DeviceLayout {
    /// A device whose own coordinate (0, 0) sits at `origin` in the shared
    /// plane. Checked when it is added to a [`Layout`].
    pub fn new(id: DeviceId, origin: Point, displays: Vec<DisplayInfo>) -> Self {
        Self {
            id,
            origin,
            displays,
        }
    }

    /// The device this entry describes.
    pub fn id(&self) -> &DeviceId {
        &self.id
    }

    /// Where the device's own (0, 0) sits in the shared plane.
    pub fn origin(&self) -> Point {
        self.origin
    }

    /// The device's displays, in its own coordinates and in the order given.
    pub fn displays(&self) -> &[DisplayInfo] {
        &self.displays
    }

    /// The smallest rectangle around all displays, in the device's own
    /// coordinates, or `None` without displays.
    pub fn bounds(&self) -> Option<Rect> {
        bounds(&self.displays)
    }

    /// The first display that contains `p` (device coordinates).
    pub fn display_at(&self, p: Point) -> Option<&DisplayInfo> {
        self.displays.iter().find(|d| d.frame.contains(p))
    }

    fn global(&self, frame: &Rect) -> Rect {
        frame.offset(self.origin.x, self.origin.y)
    }
}

/// Where the cursor goes when it leaves a display: the device and the entry
/// point in that device's own coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Crossing {
    /// The device the cursor moves to.
    pub to: DeviceId,
    /// Where it appears there.
    pub entry: EntryPoint,
}

/// A part of one display's edge from which the cursor crosses to another
/// device, in the display's own device's coordinates
/// ([`Layout::edges_toward`]).
#[derive(Clone, Debug, PartialEq)]
pub struct SharedEdge {
    /// The display's id.
    pub display: u32,
    /// The display's frame.
    pub frame: Rect,
    /// The edge of `frame` the part lies on.
    pub edge: Edge,
    /// Where the part starts along the edge: y for a left or right edge, x
    /// for a top or bottom edge.
    pub start: f64,
    /// Where it ends along the edge, after `start`.
    pub end: f64,
}

impl SharedEdge {
    /// Whether a pointer motion by `(dx, dy)` that ends at `to` (both in the
    /// same coordinates as the edge) heads for this part of the edge and
    /// ends near it: it moves toward the edge (its component along the
    /// edge's outward normal is positive), and `to` lies on the edge's
    /// display, within the part's span (ends included) and at most `reach`
    /// from the edge line. Anything non-finite never counts. The nudge rule
    /// of [`Action::Nudge`](crate::Action::Nudge).
    pub(crate) fn is_approached(&self, to: Point, dx: f64, dy: f64, reach: f64) -> bool {
        let outward = match self.edge {
            Edge::Left => -dx,
            Edge::Right => dx,
            Edge::Top => -dy,
            Edge::Bottom => dy,
        };
        let (along, across) = if self.edge.is_vertical() {
            (to.y, to.x)
        } else {
            (to.x, to.y)
        };
        // Every comparison is false for NaN.
        outward > 0.0
            && self.frame.contains(to)
            && self.start <= along
            && along <= self.end
            && (self.frame.edge_line(self.edge) - across).abs() <= reach
    }
}

/// A device placed by [`Layout::two_devices`]: flush against `side` of the
/// device `of`, and re-placed whenever either one's displays change.
#[derive(Clone, Debug, PartialEq)]
struct Anchor {
    of: DeviceId,
    side: Edge,
}

/// All devices and their displays in one shared plane.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    devices: BTreeMap<DeviceId, DeviceLayout>,
    anchors: BTreeMap<DeviceId, Anchor>,
}

impl Layout {
    /// A layout with explicitly placed devices. Every origin and display is
    /// checked; display ids must be unique within a device.
    pub fn new(devices: impl IntoIterator<Item = DeviceLayout>) -> Result<Self, LayoutError> {
        let mut layout = Layout::default();
        for device in devices {
            if layout.devices.contains_key(&device.id) {
                return Err(LayoutError::DuplicateDevice(device.id));
            }
            if !is_usable_point(device.origin) {
                return Err(LayoutError::InvalidOrigin(device.id));
            }
            validate_displays(&device.id, &device.displays)?;
            layout.devices.insert(device.id.clone(), device);
        }
        Ok(layout)
    }

    /// The v1 layout: this device at the origin and `peer` on `peer_side` of
    /// it.
    ///
    /// The peer's displays are placed as a group, flush against that side of
    /// this device's display group and centered along the shared edge. Either
    /// display list may be empty while it is unknown (the peer's arrives with
    /// its `Hello`); [`Layout::set_displays`] fills it in and re-places the
    /// peer.
    pub fn two_devices(
        me: DeviceId,
        my_displays: Vec<DisplayInfo>,
        peer: DeviceId,
        peer_side: Edge,
        peer_displays: Vec<DisplayInfo>,
    ) -> Result<Self, LayoutError> {
        if me == peer {
            return Err(LayoutError::SelfNeighbor(me));
        }
        let mut layout = Layout::new([
            DeviceLayout::new(me.clone(), Point::new(0.0, 0.0), my_displays),
            DeviceLayout::new(peer.clone(), Point::new(0.0, 0.0), peer_displays),
        ])?;
        layout.anchors.insert(
            peer,
            Anchor {
                of: me,
                side: peer_side,
            },
        );
        layout.place_anchored();
        Ok(layout)
    }

    /// The entry for `id`, if it is part of the layout.
    pub fn device(&self, id: &DeviceId) -> Option<&DeviceLayout> {
        self.devices.get(id)
    }

    /// Every device, ordered by id.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceLayout> {
        self.devices.values()
    }

    /// Replaces the displays of `id`. Devices placed by
    /// [`Layout::two_devices`] move so they stay flush and centered.
    pub fn set_displays(
        &mut self,
        id: &DeviceId,
        displays: Vec<DisplayInfo>,
    ) -> Result<(), LayoutError> {
        validate_displays(id, &displays)?;
        let device = self
            .devices
            .get_mut(id)
            .ok_or_else(|| LayoutError::UnknownDevice(id.clone()))?;
        device.displays = displays;
        self.place_anchored();
        Ok(())
    }

    /// Where the cursor lands when it leaves display `display_id` of `from`
    /// through `edge` at `at` (a point on that edge, in `from`'s coordinates).
    ///
    /// Following docs/ARCHITECTURE.md ("Exit-to-entry mapping"): candidates
    /// are displays of other devices whose opposite edge lies within
    /// `tolerance` of the edge line and whose span overlaps it. The largest
    /// overlap wins, then the smallest gap, then the lowest device id and the
    /// first display listed. The position along the edge maps
    /// proportionally, so corners map to corners, and the entry point sits 1
    /// unit inside the target display.
    ///
    /// Returns `None` when nothing faces the edge.
    pub fn exit_target(
        &self,
        from: &DeviceId,
        display_id: u32,
        edge: Edge,
        at: Point,
        tolerance: f64,
    ) -> Option<Crossing> {
        if !is_usable_point(at) {
            return None;
        }
        let source = self.devices.get(from)?;
        let display = source.displays.iter().find(|d| d.id == display_id)?;
        let (device, target) = self.facing(source, display, edge, tolerance)?;

        let (d_start, d_end) = display.frame.edge_span(edge);
        let along = if edge.is_vertical() { at.y } else { at.x };
        let t = ((along - d_start) / (d_end - d_start)).clamp(0.0, 1.0);
        let (n_start, n_end) = target.frame.edge_span(edge);
        let mapped = n_start + t * (n_end - n_start);

        let entry_edge = edge.opposite();
        let frame = target.frame;
        let inside = match entry_edge {
            Edge::Left => Point::new(frame.min_x() + 1.0, mapped),
            Edge::Right => Point::new(frame.max_x() - 1.0, mapped),
            Edge::Top => Point::new(mapped, frame.min_y() + 1.0),
            Edge::Bottom => Point::new(mapped, frame.max_y() - 1.0),
        };
        Some(Crossing {
            to: device.id.clone(),
            entry: EntryPoint {
                display_id: target.id,
                edge: entry_edge,
                // Keeps the far corner on the last addressable unit.
                position: frame.clamp(inside, 1.0),
            },
        })
    }

    /// The display of another device that faces `edge` of `display` (one of
    /// `source`'s), with its device: where the cursor goes when it leaves
    /// through that edge, by the rule of [`Layout::exit_target`]. The choice
    /// is the same wherever along the edge the cursor leaves; only the entry
    /// point depends on that. `None` when nothing faces the edge.
    fn facing(
        &self,
        source: &DeviceLayout,
        display: &DisplayInfo,
        edge: Edge,
        tolerance: f64,
    ) -> Option<(&DeviceLayout, &DisplayInfo)> {
        let exit = source.global(&display.frame);
        let line = exit.edge_line(edge);
        let (start, end) = exit.edge_span(edge);

        // (overlap, gap, device, display); strict comparisons keep the first
        // of equal candidates, and devices iterate in id order.
        let mut best: Option<(f64, f64, &DeviceLayout, &DisplayInfo)> = None;
        for device in self.devices.values().filter(|d| d.id != source.id) {
            for candidate in &device.displays {
                let frame = device.global(&candidate.frame);
                let gap = (frame.edge_line(edge.opposite()) - line).abs();
                let (c_start, c_end) = frame.edge_span(edge);
                let overlap = end.min(c_end) - start.max(c_start);
                // Both comparisons are false for NaN, so broken geometry can
                // never become a candidate.
                let faces = gap <= tolerance && overlap > 0.0;
                if !faces {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((best_overlap, best_gap, _, _)) => {
                        overlap > best_overlap || (overlap == best_overlap && gap < best_gap)
                    }
                };
                if better {
                    best = Some((overlap, gap, device, candidate));
                }
            }
        }
        best.map(|(_, _, device, target)| (device, target))
    }

    /// The parts of `me`'s display edges from which the cursor crosses to
    /// `peer`, in `me`'s own coordinates: where files dragged to the edge
    /// can be dropped to send them to `peer` (docs/M4-M5-DESIGN.md, "The
    /// drop zone").
    ///
    /// An edge leads to `peer` when [`Layout::exit_target`] sends the cursor
    /// that leaves through it to a display of `peer`. Where along the edge
    /// the cursor leaves changes only where it enters, so the whole edge
    /// counts, also beside no display of `peer` (where `peer` is shorter,
    /// say), except:
    /// - where another display of `me` continues past the edge, since the
    ///   cursor moves onto that display there instead of crossing;
    /// - within `dead_zone` of either end of the edge, where the core never
    ///   crosses (the corner dead zone).
    ///
    /// Empty parts are left out. The parts come in the order `me` lists its
    /// displays, then by edge (left, right, top, bottom), then along the
    /// edge. An edge that leads to a third device leads nowhere else. Empty
    /// for a device that is not in the layout, for `peer` equal to `me`, and
    /// for a peer without displays.
    pub fn edges_toward(
        &self,
        me: &DeviceId,
        peer: &DeviceId,
        tolerance: f64,
        dead_zone: f64,
    ) -> Vec<SharedEdge> {
        let Some(mine) = self.devices.get(me) else {
            return Vec::new();
        };
        let mut shared = Vec::new();
        for display in &mine.displays {
            let frame = display.frame;
            for edge in Edge::ALL {
                // Never true for `peer` equal to `me`: the cursor only ever
                // crosses to another device.
                let leads = self
                    .facing(mine, display, edge, tolerance)
                    .is_some_and(|(device, _)| device.id == *peer);
                if !leads {
                    continue;
                }
                let line = frame.edge_line(edge);
                let (start, end) = frame.edge_span(edge);
                let mut parts = vec![(start, end)];
                for other in mine.displays.iter().filter(|d| d.id != display.id) {
                    if (other.frame.edge_line(edge.opposite()) - line).abs() <= LOCAL_TOUCH {
                        parts = cut_span(parts, other.frame.edge_span(edge));
                    }
                }
                let corners = (start + dead_zone, end - dead_zone);
                shared.extend(
                    parts
                        .into_iter()
                        .map(|(from, to)| (from.max(corners.0), to.min(corners.1)))
                        .filter(|&(from, to)| to - from > 0.0)
                        .map(|(from, to)| SharedEdge {
                            display: display.id,
                            frame,
                            edge,
                            start: from,
                            end: to,
                        }),
                );
            }
        }
        shared
    }

    fn place_anchored(&mut self) {
        let placements: Vec<(DeviceId, Point)> = self
            .anchors
            .iter()
            .filter_map(|(id, anchor)| {
                let of = self.devices.get(&anchor.of)?;
                let device = self.devices.get(id)?;
                Some((id.clone(), beside_origin(of, &device.displays, anchor.side)))
            })
            .collect();
        for (id, origin) in placements {
            if let Some(device) = self.devices.get_mut(&id) {
                device.origin = origin;
            }
        }
    }
}

/// The origin that puts `displays` flush against `side` of `anchor`'s display
/// group, centered along the shared edge.
fn beside_origin(anchor: &DeviceLayout, displays: &[DisplayInfo], side: Edge) -> Point {
    let (Some(a), Some(p)) = (anchor.bounds().map(|b| anchor.global(&b)), bounds(displays)) else {
        // Nothing to line up yet; the origin is irrelevant without displays.
        return anchor.origin;
    };
    let (ac, pc) = (a.center(), p.center());
    match side {
        Edge::Right => Point::new(a.max_x() - p.min_x(), ac.y - pc.y),
        Edge::Left => Point::new(a.min_x() - p.max_x(), ac.y - pc.y),
        Edge::Bottom => Point::new(ac.x - pc.x, a.max_y() - p.min_y()),
        Edge::Top => Point::new(ac.x - pc.x, a.min_y() - p.max_y()),
    }
}

fn bounds(displays: &[DisplayInfo]) -> Option<Rect> {
    let mut frames = displays.iter().map(|d| d.frame);
    let first = frames.next()?;
    Some(frames.fold(first, |acc, f| acc.union(&f)))
}

/// Sorted, separate intervals with `cut` taken out of them; still sorted.
fn cut_span(spans: Vec<(f64, f64)>, cut: (f64, f64)) -> Vec<(f64, f64)> {
    spans
        .into_iter()
        .flat_map(|(start, end)| [(start, end.min(cut.0)), (start.max(cut.1), end)])
        .filter(|&(start, end)| end - start > 0.0)
        .collect()
}

fn is_usable_point(p: Point) -> bool {
    [p.x, p.y]
        .iter()
        .all(|v| v.is_finite() && v.abs() <= MAX_COORDINATE)
}

fn validate_displays(device: &DeviceId, displays: &[DisplayInfo]) -> Result<(), LayoutError> {
    let mut seen = BTreeSet::new();
    for d in displays {
        if !seen.insert(d.id) {
            return Err(LayoutError::DuplicateDisplay {
                device: device.clone(),
                display: d.id,
            });
        }
        let f = d.frame;
        let usable = is_usable_point(f.origin)
            && [f.size.width, f.size.height]
                .iter()
                .all(|v| v.is_finite() && *v >= 1.0 && *v <= MAX_COORDINATE)
            && d.scale.is_finite()
            && d.scale > 0.0;
        if !usable {
            return Err(LayoutError::InvalidDisplay {
                device: device.clone(),
                display: d.id,
            });
        }
    }
    Ok(())
}

/// What one pointer motion does on a device's displays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Travel {
    /// The motion ends inside a display (possibly another one of this device).
    Inside(Point),
    /// The motion ends outside every display.
    Exits(Exit),
}

/// A motion that ends off screen (see [`travel`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Exit {
    /// The last display the motion passes through.
    pub display: u32,
    pub frame: Rect,
    /// The edge of `frame` through which the motion leaves it.
    pub edge: Edge,
    /// Where the motion crosses that edge line, within the edge's span.
    pub at: Point,
    /// Where an OS would leave the cursor; always on a display.
    pub clamped: Point,
}

impl Exit {
    /// Whether the crossing point lies within `dead_zone` of either end of
    /// the edge. Strictly within, so a dead zone of 0 never blocks.
    pub fn in_corner_dead_zone(&self, dead_zone: f64) -> bool {
        let (start, end) = self.frame.edge_span(self.edge);
        let p = if self.edge.is_vertical() {
            self.at.y
        } else {
            self.at.x
        };
        p - start < dead_zone || end - p < dead_zone
    }

    /// The crossing point moved inside the display, for putting the cursor
    /// back where it left.
    pub fn inside_point(&self) -> Point {
        self.frame.clamp(self.at, 1.0)
    }
}

/// Edges of two displays of one device count as touching when they are at
/// most this far apart, which absorbs rounding in coordinates the platform
/// converted from physical pixels.
const LOCAL_TOUCH: f64 = 0.5;

/// Follows a motion by `(dx, dy)` from `from` across `displays`.
///
/// A motion that ends off screen is followed from display to display: where
/// it leaves one display through an edge that another display continues (a
/// monitor stacked on a laptop), it carries on there, and the exit reported
/// is that of the last display it reaches. `clamped` is where an OS leaves
/// the cursor: the end point pulled back inside the desktop's bounding box
/// when that lands on a display, so a cursor slides along the outer edge
/// onto the next display, and otherwise the end point clamped into the
/// display the motion left.
///
/// `from` should lie inside a display; if it does not (a stale position after
/// a display change), it is first moved into the nearest one. Returns `None`
/// without displays or for non-finite input, so callers simply ignore such
/// motion.
pub(crate) fn travel(displays: &[DisplayInfo], from: Point, dx: f64, dy: f64) -> Option<Travel> {
    if ![from.x, from.y, dx, dy].iter().all(|v| v.is_finite()) {
        return None;
    }
    let home = nearest_display(displays, from)?;
    let from = if home.frame.contains(from) {
        from
    } else {
        home.frame.clamp(from, 1.0)
    };
    let to = Point::new(from.x + dx, from.y + dy);
    if !(to.x.is_finite() && to.y.is_finite()) {
        return None;
    }
    if displays.iter().any(|d| d.frame.contains(to)) {
        return Some(Travel::Inside(to));
    }

    // `from` is inside `home` and `to` is not, so some edge was crossed.
    let mut last = home;
    let (mut edge, mut at) = first_exit(&home.frame, from, to)?;
    // Each display is entered at most once (a segment crosses a rectangle
    // only once), which also bounds the walk on overlapping displays.
    let mut visited = vec![home.id];
    while let Some(next) = displays
        .iter()
        .find(|d| !visited.contains(&d.id) && continues(&last.frame, edge, at, &d.frame))
    {
        let Some((next_edge, next_at)) = first_exit(&next.frame, from, to) else {
            break;
        };
        visited.push(next.id);
        (last, edge, at) = (next, next_edge, next_at);
    }

    let landing = bounds(displays)
        .map(|desktop| desktop.clamp(to, 1.0))
        .filter(|p| displays.iter().any(|d| d.frame.contains(*p)));
    Some(Travel::Exits(Exit {
        display: last.id,
        frame: last.frame,
        edge,
        at,
        clamped: landing.unwrap_or_else(|| last.frame.clamp(to, 1.0)),
    }))
}

/// The edge through which the segment `from -> to` leaves `frame`, and where
/// it crosses that edge's line (kept within the edge's span), or `None` if
/// `to` is not beyond any edge in the motion's direction. Vertical edges are
/// considered first, so an exact corner counts as a left/right exit.
fn first_exit(frame: &Rect, from: Point, to: Point) -> Option<(Edge, Point)> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let mut first: Option<(f64, Edge)> = None;
    let mut consider = |t: f64, edge: Edge| {
        if t.is_finite() && first.is_none_or(|(best, _)| t < best) {
            first = Some((t, edge));
        }
    };
    if dx > 0.0 && to.x >= frame.max_x() {
        consider((frame.max_x() - from.x) / dx, Edge::Right);
    }
    if dx < 0.0 && to.x < frame.min_x() {
        consider((frame.min_x() - from.x) / dx, Edge::Left);
    }
    if dy > 0.0 && to.y >= frame.max_y() {
        consider((frame.max_y() - from.y) / dy, Edge::Bottom);
    }
    if dy < 0.0 && to.y < frame.min_y() {
        consider((frame.min_y() - from.y) / dy, Edge::Top);
    }
    let (t, edge) = first?;
    let t = t.clamp(0.0, 1.0);
    let at = if edge.is_vertical() {
        let y = (from.y + t * dy).clamp(frame.min_y(), frame.max_y());
        Point::new(frame.edge_line(edge), y)
    } else {
        let x = (from.x + t * dx).clamp(frame.min_x(), frame.max_x());
        Point::new(x, frame.edge_line(edge))
    };
    Some((edge, at))
}

/// Whether `next` carries on past `edge` of `frame` at `at`, a point on that
/// edge: its opposite edge touches the edge line and covers `at`.
fn continues(frame: &Rect, edge: Edge, at: Point, next: &Rect) -> bool {
    let gap = (next.edge_line(edge.opposite()) - frame.edge_line(edge)).abs();
    let along = if edge.is_vertical() { at.y } else { at.x };
    let (start, end) = next.edge_span(edge);
    gap <= LOCAL_TOUCH && along >= start && along < end
}

/// A usable position for `p` on `displays`: `p` itself when a display
/// contains it, otherwise `p` clamped into the `preferred` display (or the
/// nearest one), or that display's center when `p` is not finite.
pub(crate) fn place_point(
    displays: &[DisplayInfo],
    preferred: Option<u32>,
    p: Point,
) -> Option<Point> {
    let finite = p.x.is_finite() && p.y.is_finite();
    if finite && displays.iter().any(|d| d.frame.contains(p)) {
        return Some(p);
    }
    let display = preferred
        .and_then(|id| displays.iter().find(|d| d.id == id))
        .or_else(|| nearest_display(displays, p))?;
    Some(if finite {
        display.frame.clamp(p, 1.0)
    } else {
        display.frame.center()
    })
}

/// The display containing `p`, else the closest one (the first listed on a
/// tie or when `p` is not finite).
fn nearest_display(displays: &[DisplayInfo], p: Point) -> Option<&DisplayInfo> {
    let mut best: Option<(&DisplayInfo, f64)> = None;
    for d in displays {
        if d.frame.contains(p) {
            return Some(d);
        }
        let f = d.frame;
        let dx = (f.min_x() - p.x).max(p.x - f.max_x()).max(0.0);
        let dy = (f.min_y() - p.y).max(p.y - f.max_y()).max(0.0);
        let distance = dx.hypot(dy);
        // Strictly closer only, so the first display wins a tie.
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((d, distance));
        }
    }
    best.map(|(d, _)| d)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn display(id: u32, x: f64, y: f64, w: f64, h: f64, scale: f64) -> DisplayInfo {
        DisplayInfo {
            id,
            frame: Rect::new(x, y, w, h),
            scale,
            primary: x == 0.0 && y == 0.0,
        }
    }

    fn mac() -> DeviceId {
        DeviceId::new("mac")
    }

    fn pc() -> DeviceId {
        DeviceId::new("pc")
    }

    fn mac_displays() -> Vec<DisplayInfo> {
        vec![display(1, 0.0, 0.0, 1512.0, 982.0, 2.0)]
    }

    fn pc_displays() -> Vec<DisplayInfo> {
        vec![display(7, 0.0, 0.0, 1536.0, 864.0, 1.25)]
    }

    fn mac_view() -> Layout {
        Layout::two_devices(mac(), mac_displays(), pc(), Edge::Right, pc_displays()).unwrap()
    }

    fn pc_view() -> Layout {
        Layout::two_devices(pc(), pc_displays(), mac(), Edge::Left, mac_displays()).unwrap()
    }

    fn origin(layout: &Layout, id: &DeviceId) -> Point {
        layout.device(id).unwrap().origin()
    }

    #[test]
    fn peer_is_flush_and_centered() {
        let layout = mac_view();
        assert_eq!(origin(&layout, &mac()), Point::new(0.0, 0.0));
        assert_eq!(origin(&layout, &pc()), Point::new(1512.0, 59.0));
        // The PC's own view is the mirror image: same relative placement.
        let layout = pc_view();
        assert_eq!(origin(&layout, &mac()), Point::new(-1512.0, -59.0));
    }

    #[test]
    fn every_side_places_the_group_flush() {
        let expected = [
            (Edge::Right, Point::new(1512.0, 59.0)),
            (Edge::Left, Point::new(-1536.0, 59.0)),
            (Edge::Bottom, Point::new(-12.0, 982.0)),
            (Edge::Top, Point::new(-12.0, -864.0)),
        ];
        for (side, want) in expected {
            let layout =
                Layout::two_devices(mac(), mac_displays(), pc(), side, pc_displays()).unwrap();
            assert_eq!(origin(&layout, &pc()), want, "{side:?}");
        }
    }

    #[test]
    fn unknown_peer_displays_are_filled_in_later() {
        let mut layout =
            Layout::two_devices(mac(), mac_displays(), pc(), Edge::Right, vec![]).unwrap();
        assert!(layout.device(&pc()).unwrap().displays().is_empty());
        let at = Point::new(1512.0, 491.0);
        assert_eq!(layout.exit_target(&mac(), 1, Edge::Right, at, 1.0), None);
        layout.set_displays(&pc(), pc_displays()).unwrap();
        assert_eq!(origin(&layout, &pc()), Point::new(1512.0, 59.0));
        assert!(
            layout
                .exit_target(&mac(), 1, Edge::Right, at, 1.0)
                .is_some()
        );
    }

    #[test]
    fn local_display_changes_re_place_the_peer() {
        let mut layout = mac_view();
        let taller = vec![display(1, 0.0, 0.0, 1512.0, 1182.0, 2.0)];
        layout.set_displays(&mac(), taller).unwrap();
        assert_eq!(origin(&layout, &pc()), Point::new(1512.0, 159.0));
    }

    #[test]
    fn crossing_maps_proportionally_one_unit_inside() {
        let layout = mac_view();
        let c = layout
            .exit_target(&mac(), 1, Edge::Right, Point::new(1512.0, 491.0), 1.0)
            .unwrap();
        assert_eq!(c.to, pc());
        assert_eq!(c.entry.display_id, 7);
        assert_eq!(c.entry.edge, Edge::Left);
        assert_eq!(c.entry.position, Point::new(1.0, 432.0));

        let back = pc_view()
            .exit_target(&pc(), 7, Edge::Left, Point::new(0.0, 432.0), 1.0)
            .unwrap();
        assert_eq!(back.to, mac());
        assert_eq!(back.entry.edge, Edge::Right);
        assert_eq!(back.entry.position, Point::new(1511.0, 491.0));
    }

    #[test]
    fn corners_map_to_corners() {
        let layout = mac_view();
        let top = layout
            .exit_target(&mac(), 1, Edge::Right, Point::new(1512.0, 0.0), 1.0)
            .unwrap();
        assert_eq!(top.entry.position, Point::new(1.0, 0.0));
        let bottom = layout
            .exit_target(&mac(), 1, Edge::Right, Point::new(1512.0, 982.0), 1.0)
            .unwrap();
        // The far corner lands on the last addressable unit.
        assert_eq!(bottom.entry.position, Point::new(1.0, 863.0));
    }

    #[test]
    fn edges_without_a_neighbor_have_no_target() {
        let layout = mac_view();
        for (edge, at) in [
            (Edge::Left, Point::new(0.0, 400.0)),
            (Edge::Top, Point::new(700.0, 0.0)),
            (Edge::Bottom, Point::new(700.0, 982.0)),
        ] {
            assert_eq!(
                layout.exit_target(&mac(), 1, edge, at, 1.0),
                None,
                "{edge:?}"
            );
        }
        // Unknown device or display.
        let at = Point::new(1512.0, 400.0);
        assert_eq!(layout.exit_target(&pc(), 1, Edge::Right, at, 1.0), None);
        assert_eq!(layout.exit_target(&mac(), 9, Edge::Right, at, 1.0), None);
        let nan = Point::new(f64::NAN, 1.0);
        assert_eq!(layout.exit_target(&mac(), 1, Edge::Right, nan, 1.0), None);
    }

    #[test]
    fn adjacency_tolerance_is_respected() {
        let layout = Layout::new([
            DeviceLayout::new(mac(), Point::new(0.0, 0.0), mac_displays()),
            DeviceLayout::new(pc(), Point::new(1514.0, 0.0), pc_displays()),
        ])
        .unwrap();
        let at = Point::new(1512.0, 300.0);
        assert_eq!(layout.exit_target(&mac(), 1, Edge::Right, at, 1.0), None);
        assert!(
            layout
                .exit_target(&mac(), 1, Edge::Right, at, 2.0)
                .is_some()
        );
    }

    #[test]
    fn largest_overlap_wins_then_smallest_gap() {
        // Two candidate displays on the PC side: a short one touching the
        // edge exactly and a tall one 0.5 units away.
        let layout = Layout::new([
            DeviceLayout::new(mac(), Point::new(0.0, 0.0), mac_displays()),
            DeviceLayout::new(
                pc(),
                Point::new(1512.0, 0.0),
                vec![
                    display(1, 0.0, 0.0, 800.0, 300.0, 1.0),
                    display(2, 0.5, 300.0, 800.0, 682.0, 1.0),
                ],
            ),
        ])
        .unwrap();
        let at = Point::new(1512.0, 100.0);
        let c = layout.exit_target(&mac(), 1, Edge::Right, at, 1.0).unwrap();
        assert_eq!(c.entry.display_id, 2, "largest overlap");

        let tie = Layout::new([
            DeviceLayout::new(mac(), Point::new(0.0, 0.0), mac_displays()),
            DeviceLayout::new(
                pc(),
                Point::new(1512.0, 0.0),
                vec![
                    display(1, 0.5, 0.0, 800.0, 491.0, 1.0),
                    display(2, 0.0, 491.0, 800.0, 491.0, 1.0),
                ],
            ),
        ])
        .unwrap();
        let c = tie.exit_target(&mac(), 1, Edge::Right, at, 1.0).unwrap();
        assert_eq!(c.entry.display_id, 2, "equal overlap, smaller gap");
    }

    #[test]
    fn invalid_layouts_are_rejected() {
        let bad_frames = [
            display(1, f64::NAN, 0.0, 10.0, 10.0, 1.0),
            display(1, 0.0, 0.0, 0.0, 10.0, 1.0),
            display(1, 0.0, 0.0, 10.0, -5.0, 1.0),
            display(1, 0.0, 0.0, 10.0, 10.0, 0.0),
            display(1, 0.0, 0.0, 10.0, 10.0, f64::INFINITY),
            display(1, 1e12, 0.0, 10.0, 10.0, 1.0),
        ];
        for bad in bad_frames {
            let err = Layout::two_devices(mac(), vec![bad.clone()], pc(), Edge::Right, vec![]);
            assert_eq!(
                err,
                Err(LayoutError::InvalidDisplay {
                    device: mac(),
                    display: 1
                }),
                "{bad:?}"
            );
        }
        let twice = vec![mac_displays()[0].clone(), mac_displays()[0].clone()];
        assert_eq!(
            Layout::two_devices(mac(), twice, pc(), Edge::Right, vec![]),
            Err(LayoutError::DuplicateDisplay {
                device: mac(),
                display: 1
            })
        );
        assert_eq!(
            Layout::two_devices(mac(), vec![], mac(), Edge::Right, vec![]),
            Err(LayoutError::SelfNeighbor(mac()))
        );
        assert_eq!(
            Layout::new([
                DeviceLayout::new(mac(), Point::new(0.0, 0.0), vec![]),
                DeviceLayout::new(mac(), Point::new(5.0, 0.0), vec![]),
            ]),
            Err(LayoutError::DuplicateDevice(mac()))
        );
        assert_eq!(
            Layout::new([DeviceLayout::new(mac(), Point::new(f64::NAN, 0.0), vec![])]),
            Err(LayoutError::InvalidOrigin(mac()))
        );
        let mut layout = mac_view();
        assert_eq!(
            layout.set_displays(&DeviceId::new("other"), vec![]),
            Err(LayoutError::UnknownDevice(DeviceId::new("other")))
        );
        // A refused update leaves the layout untouched.
        let before = layout.clone();
        let bad = vec![display(3, 0.0, 0.0, 0.0, 0.0, 1.0)];
        assert!(layout.set_displays(&pc(), bad).is_err());
        assert_eq!(layout, before);
    }

    #[test]
    fn external_monitor_above_the_laptop() {
        // A 1920x1080 monitor centered above the laptop: the group's right
        // edge belongs to the monitor, so only the monitor faces the PC.
        let mac_two = vec![
            mac_displays()[0].clone(),
            display(2, -204.0, -1080.0, 1920.0, 1080.0, 1.0),
        ];
        let layout = Layout::two_devices(mac(), mac_two, pc(), Edge::Right, pc_displays()).unwrap();
        assert_eq!(origin(&layout, &pc()), Point::new(1716.0, -481.0));
        let from_monitor = layout
            .exit_target(&mac(), 2, Edge::Right, Point::new(1716.0, -540.0), 1.0)
            .unwrap();
        assert_eq!(from_monitor.entry.position, Point::new(1.0, 432.0));
        let laptop_edge = Point::new(1512.0, 400.0);
        assert_eq!(
            layout.exit_target(&mac(), 1, Edge::Right, laptop_edge, 1.0),
            None
        );
    }

    fn laptop() -> DisplayInfo {
        mac_displays()[0].clone()
    }

    fn shared(display: &DisplayInfo, edge: Edge, start: f64, end: f64) -> SharedEdge {
        SharedEdge {
            display: display.id,
            frame: display.frame,
            edge,
            start,
            end,
        }
    }

    #[test]
    fn the_edge_that_leads_to_the_peer_on_each_side() {
        // The whole edge on the PC's side leads to it, minus the corner dead
        // zones: on the left or right also above and below the shorter PC,
        // since the cursor crosses there too.
        let expected = [
            (Edge::Right, 10.0, 972.0),
            (Edge::Left, 10.0, 972.0),
            (Edge::Bottom, 10.0, 1502.0),
            (Edge::Top, 10.0, 1502.0),
        ];
        for (side, start, end) in expected {
            let layout =
                Layout::two_devices(mac(), mac_displays(), pc(), side, pc_displays()).unwrap();
            assert_eq!(
                layout.edges_toward(&mac(), &pc(), 1.0, 10.0),
                [shared(&laptop(), side, start, end)],
                "{side:?}"
            );
        }
        // The PC's own view: all of its left edge leads to the Mac.
        assert_eq!(
            pc_view().edges_toward(&pc(), &mac(), 1.0, 10.0),
            [shared(&pc_displays()[0], Edge::Left, 10.0, 854.0)]
        );
    }

    #[test]
    fn the_whole_edge_leads_to_a_shorter_peer() {
        let tall = display(1, 0.0, 0.0, 1000.0, 1000.0, 1.0);
        let short = display(1, 0.0, 0.0, 1000.0, 400.0, 1.0);
        let layout = Layout::two_devices(
            mac(),
            vec![tall.clone()],
            pc(),
            Edge::Right,
            vec![short.clone()],
        )
        .unwrap();
        // The peer is beside only y 300 to 700, but the cursor crosses from
        // anywhere on the edge, corners mapping to corners.
        assert_eq!(
            layout.edges_toward(&mac(), &pc(), 1.0, 10.0),
            [shared(&tall, Edge::Right, 10.0, 990.0)]
        );
        let near_the_top = Point::new(1000.0, 100.0);
        let crossing = layout
            .exit_target(&mac(), 1, Edge::Right, near_the_top, 1.0)
            .unwrap();
        assert_eq!(crossing.to, pc());
        // From the peer, in its own coordinates, its whole edge leads to the
        // taller display too.
        assert_eq!(
            layout.edges_toward(&pc(), &mac(), 1.0, 10.0),
            [shared(&short, Edge::Left, 10.0, 390.0)]
        );
    }

    #[test]
    fn the_whole_edge_leads_to_a_peer_with_several_displays() {
        let beside = |displays| {
            Layout::new([
                DeviceLayout::new(mac(), Point::new(0.0, 0.0), mac_displays()),
                DeviceLayout::new(pc(), Point::new(1512.0, 0.0), displays),
            ])
            .unwrap()
        };
        let whole = [shared(&laptop(), Edge::Right, 10.0, 972.0)];
        // Stacked, meeting at y = 491; the upper one sits 0.5 units out,
        // within the tolerance.
        let stacked = beside(vec![
            display(1, 0.5, 0.0, 800.0, 491.0, 1.0),
            display(2, 0.0, 491.0, 800.0, 491.0, 1.0),
        ]);
        assert_eq!(stacked.edges_toward(&mac(), &pc(), 1.0, 10.0), whole);
        // Mirrored: two displays with the same frame.
        let mirrored = vec![
            pc_displays()[0].clone(),
            DisplayInfo {
                id: 8,
                ..pc_displays()[0].clone()
            },
        ];
        let layout =
            Layout::two_devices(mac(), mac_displays(), pc(), Edge::Right, mirrored).unwrap();
        assert_eq!(layout.edges_toward(&mac(), &pc(), 1.0, 10.0), whole);
        // Apart: the cursor crosses from the gap between them too, to the
        // display beside more of the edge.
        let apart = beside(vec![
            display(1, 0.0, 600.0, 800.0, 382.0, 1.0),
            display(2, 0.0, 0.0, 800.0, 300.0, 1.0),
        ]);
        assert_eq!(apart.edges_toward(&mac(), &pc(), 1.0, 10.0), whole);
        let in_the_gap = Point::new(1512.0, 450.0);
        let crossing = apart
            .exit_target(&mac(), 1, Edge::Right, in_the_gap, 1.0)
            .unwrap();
        assert_eq!((crossing.to, crossing.entry.display_id), (pc(), 1));
    }

    #[test]
    fn an_edge_that_leads_to_a_third_device_does_not_lead_to_the_peer() {
        // The PC is beside the top of the Mac's right edge, but "zed" is
        // beside more of it, so the cursor crosses to zed from all of it.
        let zed = DeviceId::new("zed");
        let layout = Layout::new([
            DeviceLayout::new(mac(), Point::new(0.0, 0.0), mac_displays()),
            DeviceLayout::new(
                pc(),
                Point::new(1512.0, 0.0),
                vec![display(1, 0.0, 0.0, 800.0, 300.0, 1.0)],
            ),
            DeviceLayout::new(
                zed.clone(),
                Point::new(1512.0, 300.0),
                vec![display(1, 0.0, 0.0, 800.0, 682.0, 1.0)],
            ),
        ])
        .unwrap();
        let beside_the_pc = Point::new(1512.0, 100.0);
        let crossing = layout
            .exit_target(&mac(), 1, Edge::Right, beside_the_pc, 1.0)
            .unwrap();
        assert_eq!(crossing.to, zed);
        assert_eq!(layout.edges_toward(&mac(), &pc(), 1.0, 10.0), []);
        assert_eq!(
            layout.edges_toward(&mac(), &zed, 1.0, 10.0),
            [shared(&laptop(), Edge::Right, 10.0, 972.0)]
        );
    }

    #[test]
    fn where_another_display_of_mine_continues_past_the_edge_does_not_count() {
        // A monitor continues past the upper half of the laptop's right
        // edge: there the cursor moves onto the monitor instead of crossing.
        // The PC is placed by hand to face the whole edge.
        let monitor = display(2, 1512.0, 0.0, 800.0, 491.0, 1.0);
        let layout = Layout::new([
            DeviceLayout::new(mac(), Point::new(0.0, 0.0), vec![laptop(), monitor]),
            DeviceLayout::new(
                pc(),
                Point::new(1512.0, 0.0),
                vec![display(1, 0.0, 0.0, 1536.0, 982.0, 1.0)],
            ),
        ])
        .unwrap();
        // The dead zone trims the end of the laptop's edge, not the place
        // where the monitor stops.
        assert_eq!(
            layout.edges_toward(&mac(), &pc(), 1.0, 10.0),
            [shared(&laptop(), Edge::Right, 491.0, 972.0)]
        );
    }

    #[test]
    fn the_corner_dead_zone_trims_both_ends_of_the_edge() {
        let same = display(1, 0.0, 0.0, 1000.0, 800.0, 1.0);
        let layout = Layout::two_devices(
            mac(),
            vec![same.clone()],
            pc(),
            Edge::Right,
            vec![same.clone()],
        )
        .unwrap();
        assert_eq!(
            layout.edges_toward(&mac(), &pc(), 1.0, 0.0),
            [shared(&same, Edge::Right, 0.0, 800.0)]
        );
        assert_eq!(
            layout.edges_toward(&mac(), &pc(), 1.0, 10.0),
            [shared(&same, Edge::Right, 10.0, 790.0)]
        );
        // A dead zone that covers the whole edge leaves nothing.
        assert_eq!(layout.edges_toward(&mac(), &pc(), 1.0, 400.0), []);
        assert_eq!(layout.edges_toward(&mac(), &pc(), 1.0, 500.0), []);
    }

    #[test]
    fn a_motion_approaches_an_edge_by_heading_for_it_and_ending_near_it() {
        // The laptop's right edge, as the default corner dead zone leaves it.
        let right = shared(&laptop(), Edge::Right, 10.0, 972.0);
        let approached = |x, y, dx, dy| right.is_approached(Point::new(x, y), dx, dy, 300.0);
        // Heading right, ending 300 units from the edge or less.
        assert!(approached(1212.0, 491.0, 5.0, 0.0));
        assert!(approached(1511.5, 491.0, 5.0, 0.0));
        assert!(!approached(1211.9, 491.0, 5.0, 0.0), "too far");
        // Mostly along the edge, but a little outward, still heads for it.
        assert!(approached(1400.0, 491.0, 0.25, -40.0));
        // Away from the edge, or along it, does not.
        assert!(!approached(1400.0, 491.0, -5.0, 0.0));
        assert!(!approached(1400.0, 491.0, 0.0, 40.0));
        assert!(!approached(1400.0, 491.0, -0.25, 40.0));
        // Only beside the part, ends included: not in the corner dead zones.
        assert!(approached(1400.0, 10.0, 5.0, 0.0));
        assert!(approached(1400.0, 972.0, 5.0, 0.0));
        assert!(!approached(1400.0, 9.9, 5.0, 0.0));
        assert!(!approached(1400.0, 972.1, 5.0, 0.0));
        // Only on the edge's own display.
        assert!(!approached(1512.0, 491.0, 5.0, 0.0), "past the edge");
        assert!(!approached(1400.0, -1.0, 5.0, 0.0), "above the display");
        // Nothing non-finite counts.
        assert!(!approached(f64::NAN, 491.0, 5.0, 0.0));
        assert!(!approached(1400.0, f64::INFINITY, 5.0, 0.0));
        assert!(!approached(1400.0, 491.0, f64::NAN, 0.0));
        assert!(!right.is_approached(Point::new(1400.0, 491.0), 5.0, 0.0, f64::NAN));
    }

    #[test]
    fn each_edge_is_approached_from_its_own_side() {
        let screen = display(3, 100.0, 200.0, 1000.0, 800.0, 1.0);
        let frame = screen.frame;
        // A point 50 units inside each edge, halfway along it, and the way
        // out through that edge.
        let cases = [
            (Edge::Left, Point::new(150.0, 600.0), (-1.0, 0.0)),
            (Edge::Right, Point::new(1050.0, 600.0), (1.0, 0.0)),
            (Edge::Top, Point::new(600.0, 250.0), (0.0, -1.0)),
            (Edge::Bottom, Point::new(600.0, 950.0), (0.0, 1.0)),
        ];
        for (edge, to, (dx, dy)) in cases {
            let (start, end) = frame.edge_span(edge);
            let part = shared(&screen, edge, start, end);
            assert!(part.is_approached(to, dx, dy, 50.0), "{edge:?}");
            assert!(!part.is_approached(to, dx, dy, 49.0), "{edge:?}: too far");
            assert!(!part.is_approached(to, -dx, -dy, 50.0), "{edge:?}: away");
            assert!(!part.is_approached(to, dy, dx, 50.0), "{edge:?}: along");
        }
        // Right on a left or top edge line, even a reach of 0 counts: the
        // core turns nudges off with a nudge distance of 0 itself.
        let (start, end) = frame.edge_span(Edge::Left);
        let left = shared(&screen, Edge::Left, start, end);
        assert!(left.is_approached(Point::new(100.0, 600.0), -1.0, 0.0, 0.0));
    }

    #[test]
    fn edges_respect_the_adjacency_tolerance() {
        let layout = Layout::new([
            DeviceLayout::new(mac(), Point::new(0.0, 0.0), mac_displays()),
            DeviceLayout::new(pc(), Point::new(1514.0, 0.0), pc_displays()),
        ])
        .unwrap();
        assert_eq!(layout.edges_toward(&mac(), &pc(), 1.0, 10.0), []);
        assert_eq!(
            layout.edges_toward(&mac(), &pc(), 2.0, 10.0),
            [shared(&laptop(), Edge::Right, 10.0, 972.0)]
        );
    }

    #[test]
    fn no_edges_lead_to_a_device_that_is_not_in_the_layout() {
        let layout = mac_view();
        let other = DeviceId::new("other");
        assert_eq!(layout.edges_toward(&mac(), &other, 1.0, 10.0), []);
        assert_eq!(layout.edges_toward(&other, &pc(), 1.0, 10.0), []);
        assert_eq!(layout.edges_toward(&mac(), &mac(), 1.0, 10.0), []);
    }

    #[test]
    fn no_edges_lead_to_a_peer_without_displays() {
        let layout = Layout::two_devices(mac(), mac_displays(), pc(), Edge::Right, vec![]).unwrap();
        assert_eq!(layout.edges_toward(&mac(), &pc(), 1.0, 10.0), []);
        assert_eq!(layout.edges_toward(&pc(), &mac(), 1.0, 10.0), []);
    }

    /// `count` displays on a grid of 100 units, so that they often meet,
    /// line up or overlap. Some are moved by a quarter unit, which still
    /// counts as meeting a neighbor.
    fn any_displays(
        count: std::ops::RangeInclusive<usize>,
    ) -> impl Strategy<Value = Vec<DisplayInfo>> {
        proptest::collection::vec((0..8u8, 0..8u8, 1..6u8, 1..6u8, any::<bool>()), count).prop_map(
            |frames| {
                frames
                    .into_iter()
                    .zip(1..)
                    .map(|((x, y, w, h, moved), id)| {
                        let shift = if moved { 0.25 } else { 0.0 };
                        let (x, y) = (f64::from(x) * 100.0 + shift, f64::from(y) * 100.0 + shift);
                        let (w, h) = (f64::from(w) * 100.0, f64::from(h) * 100.0);
                        display(id, x, y, w, h, 1.0)
                    })
                    .collect()
            },
        )
    }

    /// Where another device goes: usually flush against one side of `me`'s
    /// displays (`side` 0 to 3, in the order of [`Edge::ALL`]), slid along
    /// it by `a` hundred units and moved out by a fraction of a unit (`out`),
    /// so that edges often lead to it; otherwise (`side` 4) anywhere, at
    /// `a`, `b` hundred units.
    fn place(
        me: &DeviceLayout,
        displays: &[DisplayInfo],
        (side, a, b, out): (usize, i8, i8, usize),
    ) -> Point {
        let out = [0.0, 0.5, 1.5][out];
        let (a, b) = (f64::from(a) * 100.0, f64::from(b) * 100.0);
        let Some(&side) = Edge::ALL.get(side) else {
            return Point::new(a + out, b + out);
        };
        let flush = beside_origin(me, displays, side);
        match side {
            Edge::Left => Point::new(flush.x - out, flush.y + a),
            Edge::Right => Point::new(flush.x + out, flush.y + a),
            Edge::Top => Point::new(flush.x + a, flush.y - out),
            Edge::Bottom => Point::new(flush.x + a, flush.y + out),
        }
    }

    proptest! {
        /// A point on one of this device's edges lies in a part that leads
        /// to the peer exactly when the cursor crosses to the peer there:
        /// outside the corner dead zone, where no other display of this
        /// device continues past the edge, and where `exit_target` picks a
        /// display of the peer (not of a third device, nor nothing).
        #[test]
        fn edges_lead_to_the_peer_exactly_where_the_cursor_crosses_to_it(
            mine in any_displays(1..=3),
            theirs in any_displays(1..=3),
            third in any_displays(0..=3),
            third_sorts_first in any::<bool>(),
            places in proptest::array::uniform2((0..5usize, -4..=4i8, -4..=4i8, 0..3usize)),
            tolerance in prop::sample::select(vec![0.0, 0.5, 1.0, 2.0]),
            dead_zone in 0..=120u8,
        ) {
            let me = DeviceLayout::new(mac(), Point::new(0.0, 0.0), mine.clone());
            let third_id = DeviceId::new(if third_sorts_first { "aaa" } else { "zed" });
            let layout = Layout::new([
                DeviceLayout::new(pc(), place(&me, &theirs, places[0]), theirs),
                DeviceLayout::new(third_id, place(&me, &third, places[1]), third),
                me,
            ])
            .unwrap();
            let dead_zone = f64::from(dead_zone);
            let parts = layout.edges_toward(&mac(), &pc(), tolerance, dead_zone);

            // In order, each on its display's own edge, separate and not
            // empty.
            let rank = |part: &SharedEdge| {
                let display = mine.iter().position(|d| d.id == part.display);
                let edge = Edge::ALL.iter().position(|&e| e == part.edge);
                (display.expect("a display of mine"), edge.expect("an edge"))
            };
            for pair in parts.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                prop_assert!(rank(a) < rank(b) || (rank(a) == rank(b) && a.end < b.start));
            }
            for part in &parts {
                let frame = mine[rank(part).0].frame;
                let (start, end) = frame.edge_span(part.edge);
                prop_assert_eq!(part.frame, frame);
                prop_assert!(start <= part.start && part.start < part.end && part.end <= end);
            }

            for display in &mine {
                let frame = display.frame;
                for edge in Edge::ALL {
                    let (start, end) = frame.edge_span(edge);
                    let line = frame.edge_line(edge);
                    for i in 0..64u8 {
                        // Never within 1/64 of where a part starts or ends,
                        // which all lie on quarter units here.
                        let along = start + (end - start) * (f64::from(i) + 0.37) / 64.0;
                        let at = if edge.is_vertical() {
                            Point::new(line, along)
                        } else {
                            Point::new(along, line)
                        };
                        let exit = Exit {
                            display: display.id,
                            frame,
                            edge,
                            at,
                            clamped: at,
                        };
                        let continued = mine.iter().any(|other| {
                            other.id != display.id && continues(&frame, edge, at, &other.frame)
                        });
                        let crosses = !exit.in_corner_dead_zone(dead_zone)
                            && !continued
                            && layout
                                .exit_target(&mac(), display.id, edge, at, tolerance)
                                .is_some_and(|crossing| crossing.to == pc());
                        let in_a_part = parts.iter().any(|part| {
                            part.display == display.id
                                && part.edge == edge
                                && part.start <= along
                                && along <= part.end
                        });
                        prop_assert_eq!(
                            in_a_part,
                            crosses,
                            "display {} {:?} at {}: {:?}",
                            display.id,
                            edge,
                            along,
                            parts
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn travel_inside_and_onto_other_displays() {
        let two = vec![laptop(), display(2, 0.0, -1080.0, 1920.0, 1080.0, 1.0)];
        let from = Point::new(100.0, 10.0);
        assert_eq!(
            travel(&two, from, 5.0, -3.0),
            Some(Travel::Inside(Point::new(105.0, 7.0)))
        );
        // Up across the shared edge onto the monitor: a local move.
        assert_eq!(
            travel(&two, from, 0.0, -50.0),
            Some(Travel::Inside(Point::new(100.0, -40.0)))
        );
    }

    #[test]
    fn travel_reports_the_first_edge_crossed() {
        let one = vec![laptop()];
        let Some(Travel::Exits(exit)) = travel(&one, Point::new(1500.0, 400.0), 24.0, 10.0) else {
            panic!("expected an exit");
        };
        assert_eq!(exit.display, 1);
        assert_eq!(exit.edge, Edge::Right);
        assert_eq!(exit.at, Point::new(1512.0, 405.0));
        assert_eq!(exit.clamped, Point::new(1511.0, 410.0));
        assert_eq!(exit.inside_point(), Point::new(1511.0, 405.0));

        // Steep diagonal near the bottom-right corner: the bottom is hit first.
        let Some(Travel::Exits(exit)) = travel(&one, Point::new(1500.0, 970.0), 20.0, 40.0) else {
            panic!("expected an exit");
        };
        assert_eq!(exit.edge, Edge::Bottom);
        assert_eq!(exit.at, Point::new(1506.0, 982.0));

        // Leaving through the top and left edges.
        let Some(Travel::Exits(exit)) = travel(&one, Point::new(10.0, 5.0), -1.0, -20.0) else {
            panic!("expected an exit");
        };
        assert_eq!(exit.edge, Edge::Top);
        let Some(Travel::Exits(exit)) = travel(&one, Point::new(3.0, 500.0), -10.0, 0.0) else {
            panic!("expected an exit");
        };
        assert_eq!(exit.edge, Edge::Left);
        assert_eq!(exit.at, Point::new(0.0, 500.0));
    }

    #[test]
    fn travel_from_outside_starts_at_the_nearest_display() {
        let one = vec![laptop()];
        // A stale position beyond the right edge is pulled back in first.
        let Some(Travel::Exits(exit)) = travel(&one, Point::new(1700.0, 400.0), 5.0, 0.0) else {
            panic!("expected an exit");
        };
        assert_eq!(exit.at, Point::new(1512.0, 400.0));
        assert_eq!(
            travel(&one, Point::new(1700.0, 400.0), -100.0, 0.0),
            Some(Travel::Inside(Point::new(1411.0, 400.0)))
        );
    }

    fn exits(displays: &[DisplayInfo], from: Point, dx: f64, dy: f64) -> Exit {
        match travel(displays, from, dx, dy) {
            Some(Travel::Exits(exit)) => exit,
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[test]
    fn travel_slides_along_the_desktop_edge_onto_the_next_display() {
        // Two screens side by side, top-aligned; the second one is taller.
        let side_by_side = vec![
            display(1, 0.0, 0.0, 1536.0, 864.0, 1.25),
            display(2, 1536.0, 0.0, 1920.0, 1080.0, 1.0),
        ];
        // Right along the top edge, pushing slightly up: the motion leaves
        // the first screen through its top, where nothing is, and the cursor
        // ends up on the second screen's top row, as an OS puts it.
        let exit = exits(&side_by_side, Point::new(1531.0, 0.0), 15.0, -2.0);
        assert_eq!((exit.display, exit.edge), (1, Edge::Top));
        assert_eq!(exit.clamped, Point::new(1546.0, 0.0));
        // Below the first screen's bottom, beside the taller one, nothing is
        // on screen; the cursor stays on the display it was on.
        let exit = exits(&side_by_side, Point::new(1600.0, 1000.0), -100.0, 0.0);
        assert_eq!((exit.display, exit.edge), (2, Edge::Left));
        assert_eq!(exit.clamped, Point::new(1536.0, 1000.0));
    }

    #[test]
    fn travel_follows_the_motion_onto_a_stacked_display() {
        // A monitor above the laptop, right-aligned with it.
        let stacked = vec![laptop(), display(2, -408.0, -1080.0, 1920.0, 1080.0, 1.0)];
        // Up and right near the laptop's top-right corner: the motion passes
        // onto the monitor and leaves through the monitor's right edge.
        let exit = exits(&stacked, Point::new(1505.0, 3.0), 10.0, -10.0);
        assert_eq!((exit.display, exit.edge), (2, Edge::Right));
        assert_eq!(exit.at, Point::new(1512.0, -4.0));
        assert_eq!(exit.clamped, Point::new(1511.0, -7.0));
        // A steeper motion leaves through the laptop's own right edge first.
        let exit = exits(&stacked, Point::new(1505.0, 900.0), 20.0, -1000.0);
        assert_eq!((exit.display, exit.edge), (1, Edge::Right));
        assert_eq!(exit.at, Point::new(1512.0, 550.0));
        // Left of the laptop, below the monitor, nothing is on screen.
        let exit = exits(&stacked, Point::new(5.0, 100.0), -20.0, 0.0);
        assert_eq!((exit.display, exit.edge), (1, Edge::Left));
        assert_eq!(exit.clamped, Point::new(0.0, 100.0));
    }

    #[test]
    fn travel_ignores_unusable_input() {
        let one = vec![laptop()];
        let p = Point::new(10.0, 10.0);
        assert_eq!(travel(&[], p, 1.0, 1.0), None);
        assert_eq!(travel(&one, p, f64::NAN, 1.0), None);
        assert_eq!(travel(&one, p, 1.0, f64::INFINITY), None);
        assert_eq!(travel(&one, Point::new(f64::NAN, 0.0), 1.0, 1.0), None);
        assert!(travel(&one, p, f64::MAX, f64::MAX).is_some());
    }

    #[test]
    fn corner_dead_zone_is_strict() {
        let exit = |y: f64| Exit {
            display: 1,
            frame: laptop().frame,
            edge: Edge::Right,
            at: Point::new(1512.0, y),
            clamped: Point::new(1511.0, y),
        };
        assert!(exit(0.0).in_corner_dead_zone(10.0));
        assert!(exit(9.9).in_corner_dead_zone(10.0));
        assert!(!exit(10.0).in_corner_dead_zone(10.0));
        assert!(!exit(972.0).in_corner_dead_zone(10.0));
        assert!(exit(972.5).in_corner_dead_zone(10.0));
        assert!(!exit(0.0).in_corner_dead_zone(0.0));
        assert!(!exit(982.0).in_corner_dead_zone(0.0));
    }

    #[test]
    fn placing_points() {
        let two = vec![laptop(), display(2, 0.0, -1080.0, 1920.0, 1080.0, 1.0)];
        let inside = Point::new(1700.0, -10.0);
        assert_eq!(place_point(&two, Some(1), inside), Some(inside));
        assert_eq!(
            place_point(&two, Some(1), Point::new(5000.0, 500.0)),
            Some(Point::new(1511.0, 500.0))
        );
        assert_eq!(
            place_point(&two, None, Point::new(5000.0, -500.0)),
            Some(Point::new(1919.0, -500.0))
        );
        assert_eq!(
            place_point(&two, Some(9), Point::new(f64::NAN, 0.0)),
            Some(Point::new(756.0, 491.0))
        );
        assert_eq!(place_point(&[], None, inside), None);
    }
}
