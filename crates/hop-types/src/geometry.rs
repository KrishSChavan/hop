use serde::{Deserialize, Serialize};

/// A point in logical units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A size in logical units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle in logical units, y growing downward.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: Point::new(x, y),
            size: Size::new(width, height),
        }
    }

    pub fn min_x(&self) -> f64 {
        self.origin.x
    }

    pub fn max_x(&self) -> f64 {
        self.origin.x + self.size.width
    }

    pub fn min_y(&self) -> f64 {
        self.origin.y
    }

    pub fn max_y(&self) -> f64 {
        self.origin.y + self.size.height
    }

    pub fn center(&self) -> Point {
        Point::new(
            self.origin.x + self.size.width / 2.0,
            self.origin.y + self.size.height / 2.0,
        )
    }

    /// Whether `p` lies inside. The left and top edges are inclusive, the
    /// right and bottom edges exclusive, so adjacent rectangles never share a
    /// point.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x() && p.x < self.max_x() && p.y >= self.min_y() && p.y < self.max_y()
    }

    /// This rectangle moved by `(dx, dy)`.
    pub fn offset(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(
            self.origin.x + dx,
            self.origin.y + dy,
            self.size.width,
            self.size.height,
        )
    }

    /// The smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        let min_x = self.min_x().min(other.min_x());
        let min_y = self.min_y().min(other.min_y());
        let max_x = self.max_x().max(other.max_x());
        let max_y = self.max_y().max(other.max_y());
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }

    /// `p` clamped into the rectangle, staying at least `inset` units away
    /// from the right and bottom edges (use 1.0 for "last addressable unit").
    pub fn clamp(&self, p: Point, inset: f64) -> Point {
        let max_x = (self.max_x() - inset).max(self.min_x());
        let max_y = (self.max_y() - inset).max(self.min_y());
        Point::new(
            p.x.clamp(self.min_x(), max_x),
            p.y.clamp(self.min_y(), max_y),
        )
    }

    /// The fixed coordinate of an edge line: x for left/right edges, y for
    /// top/bottom edges.
    pub fn edge_line(&self, edge: Edge) -> f64 {
        match edge {
            Edge::Left => self.min_x(),
            Edge::Right => self.max_x(),
            Edge::Top => self.min_y(),
            Edge::Bottom => self.max_y(),
        }
    }

    /// The interval an edge covers along its own direction: `(min_y, max_y)`
    /// for left/right edges, `(min_x, max_x)` for top/bottom edges.
    pub fn edge_span(&self, edge: Edge) -> (f64, f64) {
        if edge.is_vertical() {
            (self.min_y(), self.max_y())
        } else {
            (self.min_x(), self.max_x())
        }
    }
}

/// A side of a rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    pub const fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// Left and right edges are vertical lines; top and bottom are horizontal.
    pub const fn is_vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

/// One physical display of a device, in that device's own logical coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Platform display id, stable while the display stays connected.
    pub id: u32,
    pub frame: Rect,
    /// Physical pixels per logical unit (2.0 on Retina, 1.25 at 125% scaling).
    pub scale: f64,
    pub primary: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(99.9, 49.9)));
        assert!(!r.contains(Point::new(100.0, 10.0)));
        assert!(!r.contains(Point::new(10.0, 50.0)));
    }

    #[test]
    fn clamp_respects_inset() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert_eq!(r.clamp(Point::new(500.0, -3.0), 1.0), Point::new(99.0, 0.0));
    }

    #[test]
    fn edges_and_spans() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.edge_line(Edge::Right), 110.0);
        assert_eq!(r.edge_span(Edge::Right), (20.0, 70.0));
        assert_eq!(r.edge_span(Edge::Top), (10.0, 110.0));
        assert_eq!(Edge::Left.opposite(), Edge::Right);
    }

    #[test]
    fn union_covers_both() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(20.0, -5.0, 5.0, 5.0);
        assert_eq!(a.union(&b), Rect::new(0.0, -5.0, 25.0, 15.0));
    }
}
