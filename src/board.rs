//! Reads the live board into the geometry this tool reasons about.
//!
//! The target net and every other net are treated asymmetrically on purpose.
//! For the target net we use KiCad's own zone *fill*, which already encodes
//! clearance to foreign copper, thermal reliefs and board-edge clearance — so
//! "inside the fill" is most of the work done for us. For foreign nets the fill
//! tells us where copper is, not where a new via may go, so we need the item
//! geometry itself.

use anyhow::{Context, Result, bail};
use kicad_ipc_rs::{
    BoardLayerInfo, KiCadClientBlocking, PcbItem, PcbObjectTypeCode, PolyLineNm,
    PolyLineNodeGeometryNm,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::geometry::{self, Pt, Region};

/// Foreign copper per layer, every drill hole on the board, and every via.
type CopperItems = (BTreeMap<i32, Region>, Vec<Hole>, Vec<ExistingVia>);
use crate::ipc;
use crate::kicad_proto as kp;

/// A drill hole, from any net. Vias must not collide with these even when the
/// plane fill floods over them, which it does for same-net pads.
#[derive(Debug, Clone, Copy)]
pub struct Hole {
    pub center: Pt,
    pub radius_nm: f64,
}

#[derive(Debug, Clone)]
pub struct ExistingVia {
    pub id: String,
    pub center: Pt,
}

pub struct BoardSnapshot {
    /// Target-net zone fill, per copper layer. Only layers that actually carry
    /// a zone on the net appear.
    pub target_fill: BTreeMap<i32, Region>,
    /// Whether every target-net zone reported itself as filled.
    pub all_target_zones_filled: bool,
    pub target_zone_count: usize,
    /// Foreign-net zone fill per layer, for layers set to `avoid`.
    pub foreign_fill: BTreeMap<i32, Region>,
    /// Copper of other nets (tracks, arcs, pads) per layer.
    pub foreign_copper: BTreeMap<i32, Region>,
    /// Rule areas that forbid vias, unioned across layers: a through via is
    /// blocked by such an area on any layer it passes through.
    pub via_keepouts: Region,
    pub holes: Vec<Hole>,
    pub vias: Vec<ExistingVia>,
    pub board_outline: Region,
}

pub struct ReadOptions<'a> {
    pub net: &'a str,
    /// Layers the via connects on. Foreign copper is only gathered for the
    /// other copper layers: on a connect layer the target fill already
    /// excludes foreign copper plus its clearance.
    pub connect_layers: &'a BTreeSet<i32>,
    /// Layers where foreign zone fill should block placement.
    pub avoid_fill_layers: &'a BTreeSet<i32>,
    pub need_board_outline: bool,
}

pub fn copper_layers(client: &KiCadClientBlocking) -> Result<Vec<(i32, String)>> {
    let enabled = ipc::with_busy_retry("get board layers", || client.get_board_enabled_layers())?;
    let mut out = Vec::new();
    for layer in enabled.layers {
        if is_copper(layer.id) {
            let name = BoardLayerInfo::canonical_name_for_id(layer.id)
                .unwrap_or_else(|| format!("layer{}", layer.id));
            out.push((layer.id, name));
        }
    }
    out.sort_by_key(|(id, _)| *id);
    if out.is_empty() {
        bail!("board reports no enabled copper layers");
    }
    Ok(out)
}

/// Copper layers occupy the contiguous IPC enum range F.Cu..=B.Cu.
fn is_copper(layer_id: i32) -> bool {
    (kp::BOARD_LAYER_F_CU..=kp::BOARD_LAYER_B_CU).contains(&layer_id)
}

/// The target-net copper layers, discovered from where its zones actually are.
/// This is what `connect.layers = "auto"` resolves to.
pub fn layers_with_target_zones(client: &KiCadClientBlocking, net: &str) -> Result<BTreeSet<i32>> {
    let zones = read_zones(client)?;
    let mut layers = BTreeSet::new();
    for zone in &zones {
        if zone.r#type != kp::ZONE_TYPE_COPPER || !zone_is_on_net(zone, net) {
            continue;
        }
        for fill in &zone.filled_polygons {
            if is_copper(fill.layer) {
                layers.insert(fill.layer);
            }
        }
        for layer in &zone.layers {
            if is_copper(*layer) {
                layers.insert(*layer);
            }
        }
    }
    Ok(layers)
}

pub fn read(client: &KiCadClientBlocking, opts: &ReadOptions<'_>) -> Result<BoardSnapshot> {
    let copper_layers = copper_layers(client)?;
    let zones = read_zones(client)?;

    let mut target_rings: BTreeMap<i32, Vec<Vec<(f64, f64)>>> = BTreeMap::new();
    let mut foreign_rings: BTreeMap<i32, Vec<Vec<(f64, f64)>>> = BTreeMap::new();
    let mut keepout_rings: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut all_target_zones_filled = true;
    let mut target_zone_count = 0usize;

    for zone in &zones {
        match zone.r#type {
            kp::ZONE_TYPE_RULE_AREA => {
                let forbids_vias = zone
                    .rule_area_settings
                    .map(|s| s.keepout_vias)
                    .unwrap_or(false);
                if forbids_vias
                    && let Some(outline) = &zone.outline {
                        keepout_rings.extend(geometry::polyset_rings(outline));
                    }
            }
            kp::ZONE_TYPE_COPPER => {
                let is_target = zone_is_on_net(zone, opts.net);
                if is_target {
                    target_zone_count += 1;
                    if !zone.filled {
                        all_target_zones_filled = false;
                    }
                }
                for fill in &zone.filled_polygons {
                    if !is_copper(fill.layer) {
                        continue;
                    }
                    let Some(shapes) = &fill.shapes else { continue };
                    if is_target {
                        target_rings
                            .entry(fill.layer)
                            .or_default()
                            .extend(geometry::polyset_rings(shapes));
                    } else if opts.avoid_fill_layers.contains(&fill.layer) {
                        foreign_rings
                            .entry(fill.layer)
                            .or_default()
                            .extend(geometry::polyset_rings(shapes));
                    }
                }
            }
            _ => {}
        }
    }

    let target_fill = target_rings
        .into_iter()
        .map(|(layer, rings)| (layer, geometry::normalize(rings)))
        .collect();
    let foreign_fill = foreign_rings
        .into_iter()
        .map(|(layer, rings)| (layer, geometry::normalize(rings)))
        .collect();
    let via_keepouts = geometry::normalize(keepout_rings);

    let traversed: BTreeSet<i32> = copper_layers
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !opts.connect_layers.contains(id))
        .collect();

    let (foreign_copper, holes, vias) = read_copper_items(client, opts.net, &traversed)?;

    let board_outline = if opts.need_board_outline {
        read_board_outline(client)?
    } else {
        Region::empty()
    };

    Ok(BoardSnapshot {
        target_fill,
        all_target_zones_filled,
        target_zone_count,
        foreign_fill,
        foreign_copper,
        via_keepouts,
        holes,
        vias,
        board_outline,
    })
}

/// A copper zone's net lives in its copper settings, not on the zone itself.
fn zone_is_on_net(zone: &kp::Zone, net: &str) -> bool {
    zone.copper_settings
        .as_ref()
        .and_then(|s| s.net.as_ref())
        .map(|n| n.name == net)
        .unwrap_or(false)
}

fn read_zones(client: &KiCadClientBlocking) -> Result<Vec<kp::Zone>> {
    let raw = ipc::with_busy_retry("get zones", || {
        client.get_items_raw_by_type_codes(vec![PcbObjectTypeCode::new_zone().code])
    })?;
    let mut out = Vec::with_capacity(raw.len());
    for any in &raw {
        let zone = kp::unpack_any::<kp::Zone>(any, kp::ZONE_TYPE_URL)
            .with_context(|| format!("decoding a zone payload ({})", any.type_url))?;
        out.push(zone);
    }
    Ok(out)
}

/// Tracks, arcs, pads and vias: foreign copper per traversed layer, plus every
/// drill hole on the board regardless of net.
fn read_copper_items(
    client: &KiCadClientBlocking,
    net: &str,
    traversed: &BTreeSet<i32>,
) -> Result<CopperItems> {
    let mut rings: BTreeMap<i32, Vec<Vec<(f64, f64)>>> = BTreeMap::new();
    let mut holes = Vec::new();
    let mut vias = Vec::new();

    let tracks = ipc::with_busy_retry("get tracks and arcs", || {
        client.get_items_by_type_codes(vec![
            PcbObjectTypeCode::new_trace().code,
            PcbObjectTypeCode::new_arc().code,
        ])
    })?;
    for item in tracks {
        match item {
            PcbItem::Track(t) => {
                if net_name(&t.net) == net || !traversed.contains(&t.layer.id) {
                    continue;
                }
                let (Some(s), Some(e), Some(w)) = (t.start_nm, t.end_nm, t.width_nm) else {
                    continue;
                };
                rings.entry(t.layer.id).or_default().extend(
                    geometry::thick_segment_rings(
                        Pt::new(s.x_nm as f64, s.y_nm as f64),
                        Pt::new(e.x_nm as f64, e.y_nm as f64),
                        w as f64,
                    ),
                );
            }
            PcbItem::Arc(a) => {
                if net_name(&a.net) == net || !traversed.contains(&a.layer.id) {
                    continue;
                }
                let (Some(s), Some(m), Some(e), Some(w)) =
                    (a.start_nm, a.mid_nm, a.end_nm, a.width_nm)
                else {
                    continue;
                };
                let pts = geometry::flatten_arc(
                    Pt::new(s.x_nm as f64, s.y_nm as f64),
                    Pt::new(m.x_nm as f64, m.y_nm as f64),
                    Pt::new(e.x_nm as f64, e.y_nm as f64),
                );
                for w2 in pts.windows(2) {
                    rings.entry(a.layer.id).or_default().extend(
                        geometry::thick_segment_rings(
                            Pt::new(w2[0].0, w2[0].1),
                            Pt::new(w2[1].0, w2[1].1),
                            w as f64,
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    // Vias come from the raw payload: the typed model omits the padstack's
    // copper sizes, which we need for both obstacle extent and drill radius.
    let raw_vias = ipc::with_busy_retry("get vias", || {
        client.get_items_raw_by_type_codes(vec![PcbObjectTypeCode::new_via().code])
    })?;
    for any in &raw_vias {
        let Some(via) = kp::unpack_any::<kp::Via>(any, kp::VIA_TYPE_URL) else {
            continue;
        };
        let Some(pos) = via.position else { continue };
        let center = Pt::new(pos.x_nm as f64, pos.y_nm as f64);
        if let Some(id) = via.id.as_ref() {
            vias.push(ExistingVia {
                id: id.value.clone(),
                center,
            });
        }
        if let Some(stack) = &via.pad_stack {
            if let Some(drill) = stack.drill.and_then(|d| d.diameter) {
                holes.push(Hole {
                    center,
                    radius_nm: drill.x_nm.max(drill.y_nm) as f64 / 2.0,
                });
            }
            let via_net = via.net.as_ref().map(|n| n.name.as_str()).unwrap_or("");
            if via_net != net {
                let radius = stack
                    .copper_layers
                    .iter()
                    .filter_map(|l| l.size)
                    .map(|s| s.x_nm.max(s.y_nm) as f64 / 2.0)
                    .fold(0.0_f64, f64::max);
                if radius > 0.0 {
                    for layer in traversed {
                        rings
                            .entry(*layer)
                            .or_default()
                            .push(geometry::circle_ring(center, radius, 24));
                    }
                }
            }
        }
    }

    read_pads(client, net, traversed, &mut rings, &mut holes)?;

    let foreign_copper = rings
        .into_iter()
        .map(|(layer, r)| (layer, geometry::normalize(r)))
        .collect();
    Ok((foreign_copper, holes, vias))
}

fn read_pads(
    client: &KiCadClientBlocking,
    net: &str,
    traversed: &BTreeSet<i32>,
    rings: &mut BTreeMap<i32, Vec<Vec<(f64, f64)>>>,
    holes: &mut Vec<Hole>,
) -> Result<()> {
    let pads = ipc::with_busy_retry("get pads", || {
        client.get_items_by_type_codes(vec![PcbObjectTypeCode::new_pad().code])
    })?;

    let mut foreign_pad_ids = Vec::new();
    for item in pads {
        let PcbItem::Pad(pad) = item else { continue };
        if let (Some(pos), Some(stack)) = (pad.position_nm, pad.pad_stack.as_ref())
            && let Some(drill) = stack.drill.as_ref().and_then(|d| d.diameter_nm) {
                let radius = drill.x_nm.max(drill.y_nm) as f64 / 2.0;
                if radius > 0.0 {
                    holes.push(Hole {
                        center: Pt::new(pos.x_nm as f64, pos.y_nm as f64),
                        radius_nm: radius,
                    });
                }
            }
        if net_name(&pad.net) != net
            && let Some(id) = pad.id {
                foreign_pad_ids.push(id);
            }
    }

    // Real pad outlines, so non-circular and custom padstacks are respected.
    for layer in traversed {
        if foreign_pad_ids.is_empty() {
            break;
        }
        let entries = ipc::with_busy_retry("get pad shapes", || {
            client.get_pad_shape_as_polygon(foreign_pad_ids.clone(), *layer)
        })?;
        for entry in entries {
            let mut polys = Vec::new();
            if let Some(outline) = entry.polygon.outline.as_ref() {
                polys.push(polyline_nm_points(outline));
            }
            for hole in &entry.polygon.holes {
                polys.push(polyline_nm_points(hole));
            }
            rings
                .entry(*layer)
                .or_default()
                .extend(polys.into_iter().filter(|p| p.len() >= 3));
        }
    }
    Ok(())
}

fn polyline_nm_points(line: &PolyLineNm) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(line.nodes.len());
    for node in &line.nodes {
        match node {
            PolyLineNodeGeometryNm::Point(p) => out.push((p.x_nm as f64, p.y_nm as f64)),
            PolyLineNodeGeometryNm::Arc(arc) => {
                out.extend(geometry::flatten_arc(
                    Pt::new(arc.start.x_nm as f64, arc.start.y_nm as f64),
                    Pt::new(arc.mid.x_nm as f64, arc.mid.y_nm as f64),
                    Pt::new(arc.end.x_nm as f64, arc.end.y_nm as f64),
                ));
            }
        }
    }
    out
}

fn net_name(net: &Option<kicad_ipc_rs::BoardNet>) -> &str {
    net.as_ref().map(|n| n.name.as_str()).unwrap_or("")
}

/// Assembles the Edge.Cuts outline by chaining shape endpoints into closed
/// loops. Shapes are drawn in arbitrary order and direction, so they are
/// matched end-to-end within a tolerance rather than assumed sorted.
fn read_board_outline(client: &KiCadClientBlocking) -> Result<Region> {
    use kicad_ipc_rs::PcbGraphicShapeGeometry as G;

    let edge_cuts = BoardLayerInfo::id_from_name("Edge.Cuts")
        .context("this KiCad build does not know the Edge.Cuts layer")?;

    let shapes = ipc::with_busy_retry("get board shapes", || {
        client.get_items_by_type_codes(vec![PcbObjectTypeCode::new_shape().code])
    })?;

    let mut chains: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut closed: Vec<Vec<(f64, f64)>> = Vec::new();
    for item in shapes {
        let PcbItem::BoardGraphicShape(shape) = item else {
            continue;
        };
        if shape.layer.id != edge_cuts {
            continue;
        }
        let Some(geo) = shape.geometry else { continue };
        match geo {
            G::Segment { start_nm, end_nm } => {
                if let (Some(s), Some(e)) = (start_nm, end_nm) {
                    chains.push(vec![
                        (s.x_nm as f64, s.y_nm as f64),
                        (e.x_nm as f64, e.y_nm as f64),
                    ]);
                }
            }
            G::Arc {
                start_nm,
                mid_nm,
                end_nm,
            } => {
                if let (Some(s), Some(m), Some(e)) = (start_nm, mid_nm, end_nm) {
                    chains.push(geometry::flatten_arc(
                        Pt::new(s.x_nm as f64, s.y_nm as f64),
                        Pt::new(m.x_nm as f64, m.y_nm as f64),
                        Pt::new(e.x_nm as f64, e.y_nm as f64),
                    ));
                }
            }
            G::Rectangle {
                top_left_nm,
                bottom_right_nm,
                ..
            } => {
                if let (Some(tl), Some(br)) = (top_left_nm, bottom_right_nm) {
                    let (x0, y0) = (tl.x_nm as f64, tl.y_nm as f64);
                    let (x1, y1) = (br.x_nm as f64, br.y_nm as f64);
                    closed.push(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
                }
            }
            G::Circle {
                center_nm,
                radius_point_nm,
            } => {
                if let (Some(c), Some(r)) = (center_nm, radius_point_nm) {
                    let center = Pt::new(c.x_nm as f64, c.y_nm as f64);
                    let radius = center.distance_to(Pt::new(r.x_nm as f64, r.y_nm as f64));
                    closed.push(geometry::circle_ring(center, radius, 64));
                }
            }
            // Polygon geometry is not carried by the typed model, and Bezier
            // outlines are vanishingly rare on Edge.Cuts.
            G::Polygon { .. } | G::Bezier { .. } => {}
        }
    }

    closed.extend(chain_into_loops(chains));
    Ok(geometry::normalize(closed))
}

/// Endpoint-matching tolerance. KiCad snaps outline endpoints, so anything
/// beyond a micron apart is a genuine gap rather than rounding.
const OUTLINE_JOIN_TOLERANCE_NM: f64 = 1_000.0;

fn chain_into_loops(mut pieces: Vec<Vec<(f64, f64)>>) -> Vec<Vec<(f64, f64)>> {
    let close_enough = |a: (f64, f64), b: (f64, f64)| {
        (a.0 - b.0).hypot(a.1 - b.1) <= OUTLINE_JOIN_TOLERANCE_NM
    };

    let mut loops = Vec::new();
    while let Some(mut current) = pieces.pop() {
        loop {
            let head = current[0];
            let tail = current[current.len() - 1];
            if close_enough(head, tail) && current.len() >= 3 {
                break;
            }
            let Some(idx) = pieces.iter().position(|p| {
                close_enough(tail, p[0]) || close_enough(tail, p[p.len() - 1])
            }) else {
                break;
            };
            let mut next = pieces.remove(idx);
            if close_enough(tail, next[next.len() - 1]) {
                next.reverse();
            }
            current.extend(next.into_iter().skip(1));
        }
        if current.len() >= 3 {
            loops.push(current);
        }
    }
    loops
}
