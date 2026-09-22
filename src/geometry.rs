//! Polygon geometry in KiCad's native units.
//!
//! Coordinates are nanometres carried as `f64`. Clipper2's `One` scaler stores
//! them as `i64` unchanged, and `f64` represents integers exactly well past the
//! ~10^9 nm a board spans, so nothing is lost on the round trip.

use clipper2::{
    EndType, FillRule, JoinType, Paths, Point, PointInPolygonResult, difference, inflate,
    intersect, union,
};

use crate::kicad_proto as kp;

/// Arcs in zone outlines are flattened to chords no further than this from the
/// true arc. 5 µm is far below any clearance that matters and keeps vertex
/// counts sane.
const ARC_CHORD_TOLERANCE_NM: f64 = 5_000.0;

pub type Scale = clipper2::One;

pub const NM_PER_MM: f64 = 1_000_000.0;

pub fn mm_to_nm(mm: f64) -> f64 {
    mm * NM_PER_MM
}

pub fn nm_to_mm(nm: f64) -> f64 {
    nm / NM_PER_MM
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x_nm: f64,
    pub y_nm: f64,
}

impl Pt {
    pub fn new(x_nm: f64, y_nm: f64) -> Self {
        Self { x_nm, y_nm }
    }

    pub fn distance_to(self, other: Pt) -> f64 {
        (self.x_nm - other.x_nm).hypot(self.y_nm - other.y_nm)
    }
}

/// A set of polygons, holes included. Rings are stored flat, the way Clipper2
/// returns them; containment is evaluated by even-odd parity so a point in a
/// hole reads as outside without needing ring orientation to be trusted.
/// Axis-aligned bounds of one ring, kept alongside it so containment tests can
/// reject most rings without walking their vertices. Plane fills run to tens of
/// thousands of points across many rings, and the relocation sweep queries them
/// hundreds of thousands of times.
#[derive(Debug, Clone, Copy)]
struct RingBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl RingBounds {
    fn contains(&self, pt: Pt) -> bool {
        pt.x_nm >= self.min_x
            && pt.x_nm <= self.max_x
            && pt.y_nm >= self.min_y
            && pt.y_nm <= self.max_y
    }
}

#[derive(Debug, Clone, Default)]
pub struct Region {
    paths: Paths<Scale>,
    ring_bounds: Vec<RingBounds>,
}

impl Region {
    pub fn empty() -> Self {
        Self::default()
    }

    fn wrap(paths: Paths<Scale>) -> Self {
        let ring_bounds = paths
            .iter()
            .map(|path| {
                let b = path.bounds();
                RingBounds {
                    min_x: b.min.x(),
                    min_y: b.min.y(),
                    max_x: b.max.x(),
                    max_y: b.max.y(),
                }
            })
            .collect();
        Self { paths, ring_bounds }
    }

    pub fn from_rings(rings: Vec<Vec<(f64, f64)>>) -> Self {
        let rings: Vec<Vec<(f64, f64)>> = rings.into_iter().filter(|r| r.len() >= 3).collect();
        if rings.is_empty() {
            return Self::empty();
        }
        Self::wrap(Paths::<Scale>::from(rings))
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn contains(&self, pt: Pt) -> bool {
        let point = Point::<Scale>::new(pt.x_nm, pt.y_nm);
        let mut inside = 0usize;
        for (path, bounds) in self.paths.iter().zip(&self.ring_bounds) {
            if !bounds.contains(pt) {
                continue;
            }
            match path.is_point_inside(point) {
                PointInPolygonResult::IsInside | PointInPolygonResult::IsOn => inside += 1,
                PointInPolygonResult::IsOutside => {}
            }
        }
        inside % 2 == 1
    }

    pub fn bounds_nm(&self) -> Option<(Pt, Pt)> {
        if self.is_empty() {
            return None;
        }
        let b = self.paths.bounds();
        Some((
            Pt::new(b.min.x(), b.min.y()),
            Pt::new(b.max.x(), b.max.y()),
        ))
    }

    /// Positive grows the region, negative shrinks it. Shrinking by the via's
    /// outer radius plus clearance is what turns "where is there copper" into
    /// "where can a via centre go".
    pub fn offset(&self, delta_nm: f64) -> Self {
        if self.is_empty() {
            return Self::empty();
        }
        let out = inflate(
            self.paths.clone(),
            delta_nm,
            JoinType::Round,
            EndType::Polygon,
            0.0,
        );
        // Inflate can leave self-intersections behind; simplify at a tolerance
        // far finer than any clearance so it cleans up without moving edges.
        Self::wrap(out.simplify(1.0, false))
    }

    pub fn union_with(&self, other: &Region) -> Self {
        if self.is_empty() {
            return other.clone();
        }
        if other.is_empty() {
            return self.clone();
        }
        match union(self.paths.clone(), other.paths.clone(), FillRule::NonZero) {
            Ok(paths) => Self::wrap(paths),
            Err(_) => self.clone(),
        }
    }

    pub fn intersect_with(&self, other: &Region) -> Self {
        if self.is_empty() || other.is_empty() {
            return Self::empty();
        }
        match intersect(self.paths.clone(), other.paths.clone(), FillRule::NonZero) {
            Ok(paths) => Self::wrap(paths),
            Err(_) => Self::empty(),
        }
    }

    pub fn subtract(&self, other: &Region) -> Self {
        if self.is_empty() || other.is_empty() {
            return self.clone();
        }
        match difference(self.paths.clone(), other.paths.clone(), FillRule::NonZero) {
            Ok(paths) => Self::wrap(paths),
            Err(_) => self.clone(),
        }
    }

    /// Walks every ring at a fixed pitch, yielding evenly spaced points along
    /// the boundary. Used to lay the perimeter ring of vias.
    pub fn sample_boundary(&self, pitch_nm: f64) -> Vec<Pt> {
        if pitch_nm <= 0.0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for path in self.paths.iter() {
            let ring: Vec<Pt> = path.iter().map(|p| Pt::new(p.x(), p.y())).collect();
            out.extend(sample_ring(&ring, pitch_nm));
        }
        out
    }
}

/// Normalizes a set of rings through a union so orientation is consistent and
/// overlaps are merged, which the raw KiCad rings do not guarantee.
pub fn normalize(rings: Vec<Vec<(f64, f64)>>) -> Region {
    let raw = Region::from_rings(rings);
    if raw.is_empty() {
        return raw;
    }
    match union(raw.paths.clone(), Paths::<Scale>::default(), FillRule::EvenOdd) {
        Ok(paths) => Region::wrap(paths),
        Err(_) => raw,
    }
}

fn sample_ring(ring: &[Pt], pitch_nm: f64) -> Vec<Pt> {
    if ring.len() < 2 {
        return Vec::new();
    }
    let mut perimeter = 0.0;
    for i in 0..ring.len() {
        perimeter += ring[i].distance_to(ring[(i + 1) % ring.len()]);
    }
    if perimeter <= 0.0 {
        return Vec::new();
    }

    // Spread the points evenly rather than leaving a short leftover span where
    // the walk wraps around to the start.
    let count = (perimeter / pitch_nm).floor().max(1.0) as usize;
    let step = perimeter / count as f64;

    let mut out = Vec::with_capacity(count);
    let mut target = 0.0;
    let mut travelled = 0.0;
    let mut i = 0usize;
    while out.len() < count && i < ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        let seg = a.distance_to(b);
        if seg <= 0.0 {
            i += 1;
            continue;
        }
        while target <= travelled + seg && out.len() < count {
            let t = (target - travelled) / seg;
            out.push(Pt::new(
                a.x_nm + (b.x_nm - a.x_nm) * t,
                a.y_nm + (b.y_nm - a.y_nm) * t,
            ));
            target += step;
        }
        travelled += seg;
        i += 1;
    }
    out
}

/// Converts a KiCad `PolySet` into rings, flattening any arc nodes.
pub fn polyset_rings(set: &kp::PolySet) -> Vec<Vec<(f64, f64)>> {
    let mut rings = Vec::new();
    for poly in &set.polygons {
        for line in poly.outline.iter().chain(poly.holes.iter()) {
            let ring = polyline_points(line);
            if ring.len() >= 3 {
                rings.push(ring);
            }
        }
    }
    rings
}

fn polyline_points(line: &kp::PolyLine) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(line.nodes.len());
    for node in &line.nodes {
        match node.geometry {
            Some(kp::poly_line_node::Geometry::Point(p)) => {
                out.push((p.x_nm as f64, p.y_nm as f64));
            }
            Some(kp::poly_line_node::Geometry::Arc(arc)) => {
                let (Some(s), Some(m), Some(e)) = (arc.start, arc.mid, arc.end) else {
                    continue;
                };
                out.extend(flatten_arc(
                    Pt::new(s.x_nm as f64, s.y_nm as f64),
                    Pt::new(m.x_nm as f64, m.y_nm as f64),
                    Pt::new(e.x_nm as f64, e.y_nm as f64),
                ));
            }
            None => {}
        }
    }
    out
}

/// Flattens a start/mid/end circular arc to chords within
/// `ARC_CHORD_TOLERANCE_NM`. Degenerate (collinear) arcs fall back to the two
/// endpoints.
pub fn flatten_arc(start: Pt, mid: Pt, end: Pt) -> Vec<(f64, f64)> {
    let Some((center, radius)) = circumcircle(start, mid, end) else {
        return vec![(start.x_nm, start.y_nm), (end.x_nm, end.y_nm)];
    };

    let angle_of = |p: Pt| (p.y_nm - center.y_nm).atan2(p.x_nm - center.x_nm);
    let a0 = angle_of(start);
    let a1 = angle_of(mid);
    let a2 = angle_of(end);

    // Pick the sweep direction that actually passes through the midpoint.
    let normalize = |a: f64| {
        let tau = std::f64::consts::TAU;
        ((a % tau) + tau) % tau
    };
    let ccw_to_mid = normalize(a1 - a0);
    let ccw_to_end = normalize(a2 - a0);
    let sweep = if ccw_to_mid <= ccw_to_end {
        ccw_to_end
    } else {
        -(std::f64::consts::TAU - ccw_to_end)
    };

    // Chord error for a segment of angle t is r*(1 - cos(t/2)).
    let max_step = if radius > ARC_CHORD_TOLERANCE_NM {
        2.0 * (1.0 - ARC_CHORD_TOLERANCE_NM / radius).clamp(-1.0, 1.0).acos()
    } else {
        std::f64::consts::FRAC_PI_2
    };
    let steps = (sweep.abs() / max_step).ceil().max(1.0) as usize;

    (0..=steps)
        .map(|i| {
            let a = a0 + sweep * (i as f64 / steps as f64);
            (
                center.x_nm + radius * a.cos(),
                center.y_nm + radius * a.sin(),
            )
        })
        .collect()
}

fn circumcircle(a: Pt, b: Pt, c: Pt) -> Option<(Pt, f64)> {
    let d = 2.0
        * (a.x_nm * (b.y_nm - c.y_nm) + b.x_nm * (c.y_nm - a.y_nm) + c.x_nm * (a.y_nm - b.y_nm));
    if d.abs() < 1e-6 {
        return None;
    }
    let a2 = a.x_nm * a.x_nm + a.y_nm * a.y_nm;
    let b2 = b.x_nm * b.x_nm + b.y_nm * b.y_nm;
    let c2 = c.x_nm * c.x_nm + c.y_nm * c.y_nm;
    let ux = (a2 * (b.y_nm - c.y_nm) + b2 * (c.y_nm - a.y_nm) + c2 * (a.y_nm - b.y_nm)) / d;
    let uy = (a2 * (c.x_nm - b.x_nm) + b2 * (a.x_nm - c.x_nm) + c2 * (b.x_nm - a.x_nm)) / d;
    let center = Pt::new(ux, uy);
    Some((center, center.distance_to(a)))
}

/// A circle approximated as a polygon, for building obstacle regions out of
/// pads, vias and drill holes.
pub fn circle_ring(center: Pt, radius_nm: f64, segments: usize) -> Vec<(f64, f64)> {
    let segments = segments.max(8);
    (0..segments)
        .map(|i| {
            let a = std::f64::consts::TAU * (i as f64 / segments as f64);
            (
                center.x_nm + radius_nm * a.cos(),
                center.y_nm + radius_nm * a.sin(),
            )
        })
        .collect()
}

/// A segment thickened to `width_nm`, as a rectangle with rounded-ish ends
/// approximated by the surrounding cap circles. Used for track obstacles.
pub fn thick_segment_rings(a: Pt, b: Pt, width_nm: f64) -> Vec<Vec<(f64, f64)>> {
    let half = width_nm / 2.0;
    let dx = b.x_nm - a.x_nm;
    let dy = b.y_nm - a.y_nm;
    let len = dx.hypot(dy);
    let mut rings = vec![circle_ring(a, half, 16), circle_ring(b, half, 16)];
    if len > 1e-9 {
        let nx = -dy / len * half;
        let ny = dx / len * half;
        rings.push(vec![
            (a.x_nm + nx, a.y_nm + ny),
            (b.x_nm + nx, b.y_nm + ny),
            (b.x_nm - nx, b.y_nm - ny),
            (a.x_nm - nx, a.y_nm - ny),
        ]);
    }
    rings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, size: f64) -> Vec<(f64, f64)> {
        vec![(x, y), (x + size, y), (x + size, y + size), (x, y + size)]
    }

    #[test]
    fn contains_respects_holes() {
        let region = normalize(vec![square(0.0, 0.0, 100.0), square(40.0, 40.0, 20.0)]);
        assert!(region.contains(Pt::new(10.0, 10.0)), "inside the outer ring");
        assert!(!region.contains(Pt::new(50.0, 50.0)), "inside the hole");
        assert!(!region.contains(Pt::new(200.0, 200.0)), "well outside");
    }

    #[test]
    fn negative_offset_shrinks() {
        let region = normalize(vec![square(0.0, 0.0, 100.0)]);
        assert!(region.contains(Pt::new(5.0, 50.0)));
        let eroded = region.offset(-10.0);
        assert!(!eroded.contains(Pt::new(5.0, 50.0)), "5nm in is now outside");
        assert!(eroded.contains(Pt::new(50.0, 50.0)), "centre still inside");
    }

    #[test]
    fn offset_can_erase_a_small_island() {
        let region = normalize(vec![square(0.0, 0.0, 10.0)]);
        assert!(region.offset(-50.0).is_empty());
    }

    #[test]
    fn intersect_of_disjoint_is_empty() {
        let a = normalize(vec![square(0.0, 0.0, 10.0)]);
        let b = normalize(vec![square(100.0, 100.0, 10.0)]);
        assert!(a.intersect_with(&b).is_empty());
    }

    #[test]
    fn subtract_makes_a_hole() {
        let outer = normalize(vec![square(0.0, 0.0, 100.0)]);
        let inner = normalize(vec![square(40.0, 40.0, 20.0)]);
        let holed = outer.subtract(&inner);
        assert!(holed.contains(Pt::new(10.0, 10.0)));
        assert!(!holed.contains(Pt::new(50.0, 50.0)));
    }

    #[test]
    fn boundary_sampling_walks_the_perimeter_at_pitch() {
        let region = normalize(vec![square(0.0, 0.0, 100.0)]);
        let pts = region.sample_boundary(40.0);
        assert_eq!(pts.len(), 10, "400nm perimeter at 40nm pitch");

        // Points are spaced by arc length, so straight-line distance between
        // neighbours is at most the pitch and strictly less across a corner.
        for w in pts.windows(2) {
            let d = w[0].distance_to(w[1]);
            assert!(d <= 40.0 + 1e-6, "neighbours {d} apart exceeds the pitch");
        }

        // Every sample must land on the square's boundary.
        for p in &pts {
            let on_edge = [p.x_nm, p.y_nm]
                .iter()
                .any(|v| v.abs() < 1e-6 || (v - 100.0).abs() < 1e-6);
            assert!(on_edge, "{p:?} is not on the boundary");
        }
    }

    #[test]
    fn semicircle_arc_flattens_within_tolerance() {
        let r = 1_000_000.0;
        let pts = flatten_arc(
            Pt::new(r, 0.0),
            Pt::new(0.0, r),
            Pt::new(-r, 0.0),
        );
        assert!(pts.len() > 8, "got {} points", pts.len());
        for (x, y) in &pts {
            let err = (x.hypot(*y) - r).abs();
            assert!(err < ARC_CHORD_TOLERANCE_NM, "radius error {err}");
        }
        assert!((pts[0].0 - r).abs() < 1.0);
        assert!((pts[pts.len() - 1].0 + r).abs() < 1.0);
    }

    #[test]
    fn collinear_arc_falls_back_to_endpoints() {
        let pts = flatten_arc(Pt::new(0.0, 0.0), Pt::new(5.0, 0.0), Pt::new(10.0, 0.0));
        assert_eq!(pts, vec![(0.0, 0.0), (10.0, 0.0)]);
    }

}
