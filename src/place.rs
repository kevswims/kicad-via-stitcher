//! Candidate generation, obstacle filtering and nearest-fit relocation.

use std::collections::HashMap;

use crate::board::Hole;
use crate::geometry::{Pt, Region};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Not inside the plane on every connect layer, or blocked by foreign
    /// copper or a via keepout — all of which are baked into the admissible
    /// region before placement runs.
    NoAdmissibleCopper,
    DrillTooClose,
    TooCloseToAnotherVia,
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::NoAdmissibleCopper => "no plane copper clear of obstacles here",
            Self::DrillTooClose => "drill would clash with an existing hole",
            Self::TooCloseToAnotherVia => "too close to another stitching via",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Grid,
    Edge,
}

#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub at: Pt,
    pub ideal: Pt,
    pub origin: Origin,
    pub relocated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Skip {
    pub ideal: Pt,
    pub origin: Origin,
    pub reason: SkipReason,
}

pub struct Plan {
    pub placed: Vec<Placement>,
    pub skipped: Vec<Skip>,
}

pub struct Params {
    pub via_drill_radius_nm: f64,
    pub drill_to_drill_nm: f64,
    pub min_spacing_nm: f64,
    pub search_radius_nm: f64,
    /// Step for both the radial and angular sweep of the relocation search.
    pub search_step_nm: f64,
}

/// Uniform-grid point index. Cell size is the query radius, so a lookup only
/// has to visit the 3x3 neighbourhood — the whole-board O(n^2) scan the legacy
/// plugin does is far too slow once a plane holds thousands of vias.
struct PointIndex {
    cell_nm: f64,
    cells: HashMap<(i64, i64), Vec<(Pt, f64)>>,
    /// The largest radius held in the index. The neighbourhood a query has to
    /// visit depends on it: a wide pad or drill can reach a candidate from a
    /// cell several steps away, even though its centre is nowhere near.
    max_radius_nm: f64,
}

impl PointIndex {
    fn new(cell_nm: f64) -> Self {
        Self {
            cell_nm: cell_nm.max(1.0),
            cells: HashMap::new(),
            max_radius_nm: 0.0,
        }
    }

    fn key(&self, p: Pt) -> (i64, i64) {
        (
            (p.x_nm / self.cell_nm).floor() as i64,
            (p.y_nm / self.cell_nm).floor() as i64,
        )
    }

    fn insert(&mut self, p: Pt, radius_nm: f64) {
        self.max_radius_nm = self.max_radius_nm.max(radius_nm);
        self.cells.entry(self.key(p)).or_default().push((p, radius_nm));
    }

    /// True when every indexed point is at least `clearance_nm` away from `p`
    /// once both radii are accounted for.
    fn is_clear(&self, p: Pt, own_radius_nm: f64, clearance_nm: f64) -> bool {
        let (cx, cy) = self.key(p);
        let span = own_radius_nm + self.max_radius_nm + clearance_nm;
        let reach = ((span / self.cell_nm).ceil() as i64).max(1);
        for dx in -reach..=reach {
            for dy in -reach..=reach {
                let Some(bucket) = self.cells.get(&(cx + dx, cy + dy)) else {
                    continue;
                };
                for (other, other_radius) in bucket {
                    let needed = own_radius_nm + other_radius + clearance_nm;
                    if p.distance_to(*other) < needed {
                        return false;
                    }
                }
            }
        }
        true
    }
}

pub struct Candidate {
    pub at: Pt,
    pub origin: Origin,
}

/// Grid points across the admissible region's bounding box, phased on the
/// configured origin so the pattern is stable across runs and boards.
pub fn grid_candidates(
    region: &Region,
    x_spacing_nm: f64,
    y_spacing_nm: f64,
    origin_x_nm: f64,
    origin_y_nm: f64,
) -> Vec<Candidate> {
    let Some((min, max)) = region.bounds_nm() else {
        return Vec::new();
    };
    if x_spacing_nm <= 0.0 || y_spacing_nm <= 0.0 {
        return Vec::new();
    }

    let first = |lo: f64, origin: f64, step: f64| {
        origin + ((lo - origin) / step).ceil() * step
    };

    let mut out = Vec::new();
    let mut y = first(min.y_nm, origin_y_nm, y_spacing_nm);
    while y <= max.y_nm {
        let mut x = first(min.x_nm, origin_x_nm, x_spacing_nm);
        while x <= max.x_nm {
            out.push(Candidate {
                at: Pt::new(x, y),
                origin: Origin::Grid,
            });
            x += x_spacing_nm;
        }
        y += y_spacing_nm;
    }
    out
}

pub fn edge_candidates(reference: &Region, spacing_nm: f64) -> Vec<Candidate> {
    reference
        .sample_boundary(spacing_nm)
        .into_iter()
        .map(|at| Candidate {
            at,
            origin: Origin::Edge,
        })
        .collect()
}

/// Places candidates, relocating the ones that don't fit.
///
/// Candidates are placed in two passes. Everything that fits at its ideal spot
/// is committed first, then the failures are relocated against that committed
/// set. Relocation breaks grid regularity by nature, so this ordering keeps a
/// displaced via from pushing a well-placed one out of the way, and stops two
/// relocations from converging on the same gap.
pub fn plan(
    candidates: Vec<Candidate>,
    admissible: &Region,
    holes: &[Hole],
    occupied: &[Pt],
    params: &Params,
) -> Plan {
    let mut hole_index = PointIndex::new((params.via_drill_radius_nm + params.drill_to_drill_nm).max(1.0) * 2.0);
    for hole in holes {
        hole_index.insert(hole.center, hole.radius_nm);
    }

    let mut via_index = PointIndex::new(params.min_spacing_nm.max(1.0));
    for pt in occupied {
        via_index.insert(*pt, 0.0);
    }

    let fits = |pt: Pt, via_index: &PointIndex| -> Result<(), SkipReason> {
        if !admissible.contains(pt) {
            return Err(SkipReason::NoAdmissibleCopper);
        }
        if !hole_index.is_clear(pt, params.via_drill_radius_nm, params.drill_to_drill_nm) {
            return Err(SkipReason::DrillTooClose);
        }
        if !via_index.is_clear(pt, 0.0, params.min_spacing_nm) {
            return Err(SkipReason::TooCloseToAnotherVia);
        }
        Ok(())
    };

    let mut placed = Vec::new();
    let mut deferred = Vec::new();

    for candidate in candidates {
        match fits(candidate.at, &via_index) {
            Ok(()) => {
                via_index.insert(candidate.at, 0.0);
                placed.push(Placement {
                    at: candidate.at,
                    ideal: candidate.at,
                    origin: candidate.origin,
                    relocated: false,
                });
            }
            Err(reason) => deferred.push((candidate, reason)),
        }
    }

    let mut skipped = Vec::new();
    for (candidate, reason) in deferred {
        match relocate(candidate.at, params, |pt| fits(pt, &via_index).is_ok()) {
            Some(at) => {
                via_index.insert(at, 0.0);
                placed.push(Placement {
                    at,
                    ideal: candidate.at,
                    origin: candidate.origin,
                    relocated: true,
                });
            }
            None => skipped.push(Skip {
                ideal: candidate.at,
                origin: candidate.origin,
                reason,
            }),
        }
    }

    Plan { placed, skipped }
}

/// Searches concentric rings outward from `ideal`, returning the first point
/// that fits. Rings grow monotonically, so the first hit is the nearest one.
fn relocate(ideal: Pt, params: &Params, fits: impl Fn(Pt) -> bool) -> Option<Pt> {
    if params.search_radius_nm <= 0.0 {
        return None;
    }
    let step = params.search_step_nm.max(1.0);
    let rings = (params.search_radius_nm / step).floor() as usize;

    for ring in 1..=rings {
        let radius = step * ring as f64;
        // Keep arc spacing roughly equal to the radial step so coverage stays
        // uniform as the rings grow.
        let samples = ((std::f64::consts::TAU * radius / step).ceil() as usize).max(8);
        for i in 0..samples {
            let angle = std::f64::consts::TAU * (i as f64 / samples as f64);
            let pt = Pt::new(
                ideal.x_nm + radius * angle.cos(),
                ideal.y_nm + radius * angle.sin(),
            );
            if fits(pt) {
                return Some(pt);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry;

    fn square(x: f64, y: f64, size: f64) -> Vec<(f64, f64)> {
        vec![(x, y), (x + size, y), (x + size, y + size), (x, y + size)]
    }

    fn params() -> Params {
        Params {
            via_drill_radius_nm: 150_000.0,
            drill_to_drill_nm: 500_000.0,
            min_spacing_nm: 1_000_000.0,
            search_radius_nm: 2_000_000.0,
            search_step_nm: 200_000.0,
        }
    }

    #[test]
    fn grid_is_phased_on_the_configured_origin() {
        let region = geometry::normalize(vec![square(0.0, 0.0, 10_000_000.0)]);
        let c = grid_candidates(&region, 2_500_000.0, 2_500_000.0, 250_000.0, 250_000.0);
        assert!(!c.is_empty());
        for cand in &c {
            let off_x = (cand.at.x_nm - 250_000.0) % 2_500_000.0;
            assert!(off_x.abs() < 1.0, "x {} is off-grid", cand.at.x_nm);
        }
    }

    #[test]
    fn candidates_outside_the_region_are_skipped_not_placed() {
        let region = geometry::normalize(vec![square(0.0, 0.0, 1_000_000.0)]);
        let far = vec![Candidate {
            at: Pt::new(50_000_000.0, 50_000_000.0),
            origin: Origin::Grid,
        }];
        let plan = plan(far, &region, &[], &[], &params());
        assert!(plan.placed.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, SkipReason::NoAdmissibleCopper);
    }

    #[test]
    fn a_blocked_candidate_relocates_to_the_nearest_free_spot() {
        let region = geometry::normalize(vec![square(0.0, 0.0, 20_000_000.0)]);
        let ideal = Pt::new(10_000_000.0, 10_000_000.0);
        // A hole exactly at the ideal position forces a move.
        let holes = vec![Hole {
            center: ideal,
            radius_nm: 300_000.0,
        }];
        let plan = plan(
            vec![Candidate {
                at: ideal,
                origin: Origin::Grid,
            }],
            &region,
            &holes,
            &[],
            &params(),
        );
        assert_eq!(plan.placed.len(), 1);
        let p = plan.placed[0];
        assert!(p.relocated);
        let moved = p.at.distance_to(ideal);
        assert!(moved > 0.0 && moved <= 2_000_000.0, "moved {moved}");
        assert!(region.contains(p.at));
    }

    #[test]
    fn relocation_gives_up_outside_the_search_radius() {
        let region = geometry::normalize(vec![square(0.0, 0.0, 20_000_000.0)]);
        let ideal = Pt::new(10_000_000.0, 10_000_000.0);
        // A hole far larger than the search radius leaves nowhere to go.
        let holes = vec![Hole {
            center: ideal,
            radius_nm: 8_000_000.0,
        }];
        let plan = plan(
            vec![Candidate {
                at: ideal,
                origin: Origin::Grid,
            }],
            &region,
            &holes,
            &[],
            &params(),
        );
        assert!(plan.placed.is_empty());
        assert_eq!(plan.skipped[0].reason, SkipReason::DrillTooClose);
    }

    #[test]
    fn existing_vias_are_respected_as_occupants() {
        let region = geometry::normalize(vec![square(0.0, 0.0, 20_000_000.0)]);
        let ideal = Pt::new(10_000_000.0, 10_000_000.0);
        let plan = plan(
            vec![Candidate {
                at: ideal,
                origin: Origin::Grid,
            }],
            &region,
            &[],
            &[ideal],
            &params(),
        );
        // The spot is taken, so it must move rather than stack on top.
        assert_eq!(plan.placed.len(), 1);
        assert!(plan.placed[0].relocated);
        assert!(plan.placed[0].at.distance_to(ideal) >= 1_000_000.0 - 1.0);
    }

    #[test]
    fn placed_vias_keep_their_minimum_separation() {
        let region = geometry::normalize(vec![square(0.0, 0.0, 40_000_000.0)]);
        let p = params();
        let candidates = grid_candidates(&region, 1_200_000.0, 1_200_000.0, 0.0, 0.0);
        let plan = plan(candidates, &region, &[], &[], &p);
        assert!(plan.placed.len() > 10);
        for (i, a) in plan.placed.iter().enumerate() {
            for b in &plan.placed[i + 1..] {
                let d = a.at.distance_to(b.at);
                assert!(d >= p.min_spacing_nm - 1.0, "two vias only {d} apart");
            }
        }
    }
}
