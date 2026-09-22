//! `stitch` — compute and place the via field.

use anyhow::{Context, Result, bail};
use kicad_ipc_rs::{BoardLayerInfo, CommitAction, KiCadClientBlocking};
use std::collections::BTreeSet;
use std::path::Path;

use crate::board::{self, BoardSnapshot};
use crate::config::{Config, EdgeReference, TraversalPolicy};
use crate::geometry::{Pt, Region, mm_to_nm, nm_to_mm};
use crate::ipc;
use crate::kicad_proto as kp;
use crate::manifest::{self, Manifest};
use crate::place::{self, Origin, Params, Plan};

/// Batch size for `create_items`. KiCad reports one opaque error for a whole
/// batch, so keeping batches modest localizes a failure to a handful of vias.
const CREATE_BATCH: usize = 200;

pub struct Options {
    pub config_path: std::path::PathBuf,
    pub dry_run: bool,
    pub replace: bool,
    pub refill_first: bool,
    pub save: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let config = Config::load(&opts.config_path)?;
    let client = ipc::connect()?;

    if !ipc::with_busy_retry("check for an open board", || client.has_open_board())? {
        bail!("no board is open in the PCB editor");
    }

    validate_net(&client, &config)?;

    if opts.refill_first {
        println!("refilling zones before reading the board...");
        ipc::with_busy_retry("refill zones", || client.refill_all_zones())?;
    }

    let connect_layers = resolve_connect_layers(&client, &config)?;
    let layer_names: Vec<String> = connect_layers.iter().map(|id| layer_name(*id)).collect();
    println!(
        "net {:?} connects on {} layer(s): {}",
        config.net,
        connect_layers.len(),
        layer_names.join(", ")
    );

    let copper = board::copper_layers(&client)?;
    let avoid_fill_layers: BTreeSet<i32> = copper
        .iter()
        .filter(|(id, name)| {
            !connect_layers.contains(id)
                && config.traversal.policy_for(name) == TraversalPolicy::Avoid
        })
        .map(|(id, _)| *id)
        .collect();
    if !avoid_fill_layers.is_empty() {
        let names: Vec<String> = avoid_fill_layers.iter().map(|id| layer_name(*id)).collect();
        println!("avoiding foreign plane fill on: {}", names.join(", "));
    }

    let snapshot = board::read(
        &client,
        &board::ReadOptions {
            net: &config.net,
            connect_layers: &connect_layers,
            avoid_fill_layers: &avoid_fill_layers,
            need_board_outline: needs_board_outline(&config),
        },
    )?;

    if snapshot.target_zone_count == 0 {
        bail!("no copper zone on net {:?} — nothing to stitch", config.net);
    }
    if !snapshot.all_target_zones_filled {
        bail!(
            "some zones on net {:?} are not filled, so their fill geometry is stale or empty.\n\
             Fill zones in KiCad (Edit > Fill All Zones), or re-run with --refill-first.",
            config.net
        );
    }

    let (connect_region, admissible) = build_regions(&config, &snapshot, &connect_layers)?;
    if admissible.is_empty() {
        bail!(
            "no room anywhere for a {}mm via with {}mm clearance on this plane",
            config.via.diameter_mm,
            config.clearance.copper_mm
        );
    }

    let existing = existing_group_vias(&client, &config, &snapshot)?;
    let mut occupied: Vec<Pt> = Vec::new();
    if opts.replace {
        println!("--replace: {} existing stitch via(s) will be removed", existing.len());
    } else {
        occupied.extend(existing.iter().map(|v| v.center));
        if !existing.is_empty() {
            println!(
                "{} existing stitch via(s) kept; new vias will fill the gaps",
                existing.len()
            );
        }
    }

    let mut candidates = Vec::new();
    if config.grid.enabled {
        // The grid gets its own, further-in region when a setback is
        // configured, so a loose interior grid does not land on top of a
        // tightly-spaced perimeter ring.
        let grid_region = if config.grid.has_edge_setback() {
            let reference = edge_reference_region(
                &config,
                config.grid.edge_reference_or(config.edge.reference),
                &connect_region,
                &snapshot,
            )?;
            let inset = reference.offset(-mm_to_nm(config.grid.edge_distance_mm));
            let region = admissible.intersect_with(&inset);
            println!(
                "grid held {} mm back from the {} edge",
                config.grid.edge_distance_mm,
                match config.grid.edge_reference_or(config.edge.reference) {
                    EdgeReference::Board => "board",
                    EdgeReference::Plane => "plane",
                }
            );
            region
        } else {
            admissible.clone()
        };

        candidates.extend(place::grid_candidates(
            &grid_region,
            mm_to_nm(config.grid.x_spacing_mm),
            mm_to_nm(config.grid.y_spacing_mm),
            mm_to_nm(config.grid.origin_x_mm),
            mm_to_nm(config.grid.origin_y_mm),
        ));
    }
    if config.edge.enabled {
        let reference =
            edge_reference_region(&config, config.edge.reference, &connect_region, &snapshot)?;
        let inset = reference.offset(-mm_to_nm(config.edge.distance_mm));
        candidates.extend(place::edge_candidates(
            &inset,
            mm_to_nm(config.edge.spacing_mm),
        ));
    }
    println!("{} candidate position(s)", candidates.len());

    let params = Params {
        via_drill_radius_nm: mm_to_nm(config.via.drill_mm) / 2.0,
        drill_to_drill_nm: mm_to_nm(config.clearance.drill_to_drill_mm),
        min_spacing_nm: mm_to_nm(config.search.min_spacing_mm),
        search_radius_nm: mm_to_nm(config.search.radius_mm),
        search_step_nm: search_step_nm(config.search.radius_mm),
    };
    let plan = place::plan(candidates, &admissible, &snapshot.holes, &occupied, &params);

    report(&plan);

    if opts.dry_run {
        println!("\n--dry-run: board not modified");
        return Ok(());
    }
    if plan.placed.is_empty() {
        println!("\nnothing to place");
        return Ok(());
    }

    apply(&client, &config, &plan, &existing, opts)?;
    Ok(())
}

/// How many skipped positions to list individually before summarizing.
const SKIP_DETAIL_LIMIT: usize = 10;

/// Whether any part of the run measures from the Edge.Cuts outline.
fn needs_board_outline(config: &Config) -> bool {
    let ring = config.edge.enabled && config.edge.reference == EdgeReference::Board;
    let setback = config.grid.has_edge_setback()
        && config.grid.edge_reference_or(config.edge.reference) == EdgeReference::Board;
    ring || setback
}

/// The outline a distance is measured in from.
fn edge_reference_region(
    config: &Config,
    reference: EdgeReference,
    connect_region: &Region,
    snapshot: &BoardSnapshot,
) -> Result<Region> {
    match reference {
        EdgeReference::Plane => Ok(connect_region.clone()),
        EdgeReference::Board => {
            if snapshot.board_outline.is_empty() {
                bail!(
                    "reference = \"board\" was requested for net {:?}, but no closed Edge.Cuts \
                     outline could be assembled from this board",
                    config.net
                );
            }
            Ok(snapshot.board_outline.clone())
        }
    }
}

/// Resolution of the relocation sweep, radially and along each ring.
///
/// It has to divide the search radius into enough rings to be worth calling a
/// search: stepping by something as coarse as the via diameter can leave only
/// a single ring, so a via either lands one step out or is skipped. Eight rings
/// is fine enough to find real gaps and still bounded work per candidate.
fn search_step_nm(search_radius_mm: f64) -> f64 {
    const MIN_STEP_NM: f64 = 50_000.0;
    (mm_to_nm(search_radius_mm) / 8.0).max(MIN_STEP_NM)
}

fn report(plan: &Plan) {
    let grid = plan.placed.iter().filter(|p| p.origin == Origin::Grid).count();
    let edge = plan.placed.len() - grid;
    let relocated: Vec<f64> = plan
        .placed
        .iter()
        .filter(|p| p.relocated)
        .map(|p| p.at.distance_to(p.ideal))
        .collect();

    println!(
        "\nplaced {} via(s): {grid} grid, {edge} edge",
        plan.placed.len()
    );
    if let Some(max) = relocated.iter().cloned().fold(None, |m: Option<f64>, d| {
        Some(m.map_or(d, |m| m.max(d)))
    }) {
        let mean = relocated.iter().sum::<f64>() / relocated.len() as f64;
        println!(
            "  {} nudged to fit (mean {:.3} mm, worst {:.3} mm)",
            relocated.len(),
            nm_to_mm(mean),
            nm_to_mm(max)
        );
    }

    if plan.skipped.is_empty() {
        return;
    }

    println!("skipped {}:", plan.skipped.len());
    let mut counts: std::collections::BTreeMap<(&str, &str), usize> = Default::default();
    for skip in &plan.skipped {
        let origin = match skip.origin {
            Origin::Grid => "grid",
            Origin::Edge => "edge",
        };
        *counts.entry((origin, skip.reason.describe())).or_default() += 1;
    }
    for ((origin, reason), count) in counts {
        println!("  {count:>5}  {origin}: {reason}");
    }

    // Enough coordinates to find them on the board without flooding the terminal.
    for skip in plan.skipped.iter().take(SKIP_DETAIL_LIMIT) {
        println!(
            "    ({:.3}, {:.3}) mm — {}",
            nm_to_mm(skip.ideal.x_nm),
            nm_to_mm(skip.ideal.y_nm),
            skip.reason.describe()
        );
    }
    if plan.skipped.len() > SKIP_DETAIL_LIMIT {
        println!("    ... and {} more", plan.skipped.len() - SKIP_DETAIL_LIMIT);
    }
}

fn validate_net(client: &KiCadClientBlocking, config: &Config) -> Result<()> {
    let nets = ipc::with_busy_retry("get nets", || client.get_nets())?;
    if nets.iter().any(|n| n.name == config.net) {
        return Ok(());
    }
    // Creating an item on an unknown net name silently registers that net, so
    // a typo would otherwise produce a plausible-looking but useless result.
    let mut candidates: Vec<&str> = nets
        .iter()
        .map(|n| n.name.as_str())
        .filter(|n| n.to_ascii_uppercase().contains(&config.net.to_ascii_uppercase()))
        .take(5)
        .collect();
    candidates.sort();
    let hint = if candidates.is_empty() {
        String::new()
    } else {
        format!("\nDid you mean one of: {}", candidates.join(", "))
    };
    bail!("no net named {:?} on this board{hint}", config.net);
}

fn resolve_connect_layers(
    client: &KiCadClientBlocking,
    config: &Config,
) -> Result<BTreeSet<i32>> {
    match config.connect_layers() {
        Some(names) => {
            let mut ids = BTreeSet::new();
            for name in names {
                let id = BoardLayerInfo::id_from_name(name)
                    .with_context(|| format!("unknown layer name {name:?} in connect.layers"))?;
                ids.insert(id);
            }
            Ok(ids)
        }
        None => {
            let found = board::layers_with_target_zones(client, &config.net)?;
            if found.len() < 2 {
                bail!(
                    "connect.layers = \"auto\" found {} layer(s) with a {:?} zone; \
                     a stitching via needs at least 2 to tie together",
                    found.len(),
                    config.net
                );
            }
            Ok(found)
        }
    }
}

/// Returns the connect region (plane present on every connect layer) and the
/// admissible region (that, minus obstacles, eroded so the via pad fits).
fn build_regions(
    config: &Config,
    snapshot: &BoardSnapshot,
    connect_layers: &BTreeSet<i32>,
) -> Result<(Region, Region)> {
    let mut connect_region: Option<Region> = None;
    for layer in connect_layers {
        let Some(fill) = snapshot.target_fill.get(layer) else {
            bail!(
                "layer {} has no filled {:?} copper, so no via can connect on it",
                layer_name(*layer),
                config.net
            );
        };
        connect_region = Some(match connect_region {
            None => fill.clone(),
            Some(acc) => acc.intersect_with(fill),
        });
    }
    let connect_region = connect_region.unwrap_or_default();
    if connect_region.is_empty() {
        bail!(
            "the {:?} plane never overlaps across all connect layers — nothing to stitch",
            config.net
        );
    }

    // The via pad's outer radius plus clearance: the distance its centre must
    // keep from anything it may not touch.
    let keepout_nm = mm_to_nm(config.via.diameter_mm) / 2.0 + mm_to_nm(config.clearance.copper_mm);

    let mut blocked = snapshot.via_keepouts.clone();
    for region in snapshot.foreign_fill.values() {
        blocked = blocked.union_with(region);
    }
    for region in snapshot.foreign_copper.values() {
        blocked = blocked.union_with(region);
    }

    // Two independent constraints, each worth exactly one keepout:
    //   - the via pad must sit inside the plane, so shrink the connect region;
    //   - the via pad must stay off foreign copper, so grow that and cut it out.
    // Eroding the result of the subtraction instead would apply both to the
    // obstacle boundary and hold vias twice as far from power planes as asked.
    let mut admissible = connect_region.offset(-keepout_nm);
    if !blocked.is_empty() && !admissible.is_empty() {
        admissible = admissible.subtract(&blocked.offset(keepout_nm));
    }

    Ok((connect_region, admissible))
}

fn existing_group_vias(
    client: &KiCadClientBlocking,
    config: &Config,
    snapshot: &BoardSnapshot,
) -> Result<Vec<board::ExistingVia>> {
    let ids = crate::commands::clear::stitch_via_ids(client, config)?;
    Ok(snapshot
        .vias
        .iter()
        .filter(|v| ids.contains(&v.id))
        .cloned()
        .collect())
}

fn apply(
    client: &KiCadClientBlocking,
    config: &Config,
    plan: &Plan,
    existing: &[board::ExistingVia],
    opts: &Options,
) -> Result<()> {
    let net = kp::Net {
        code: None,
        name: config.net.clone(),
    };
    let diameter_nm = mm_to_nm(config.via.diameter_mm) as i64;
    let drill_nm = mm_to_nm(config.via.drill_mm) as i64;
    let ulr = config.via.unconnected_layer_removal.to_proto();

    let vias_before = count_vias(client)?;
    let session = ipc::with_busy_retry("begin commit", || client.begin_commit())?;

    let result = (|| -> Result<Vec<String>> {
        if opts.replace && !existing.is_empty() {
            let ids: Vec<String> = existing.iter().map(|v| v.id.clone()).collect();
            client.delete_items(ids)?;
        }

        let mut created_ids = Vec::with_capacity(plan.placed.len());
        for chunk in plan.placed.chunks(CREATE_BATCH) {
            let items: Vec<prost_types::Any> = chunk
                .iter()
                .map(|p| {
                    kp::pack_any(
                        &through_via(&net, p.at, diameter_nm, drill_nm, ulr),
                        kp::VIA_TYPE_URL,
                    )
                })
                .collect();
            let created = client.create_items(items, None)?;
            for any in &created {
                if let Some(id) = kp::unpack_any::<kp::Via>(any, kp::VIA_TYPE_URL)
                    .and_then(|v| v.id)
                    .map(|k| k.value)
                {
                    created_ids.push(id);
                }
            }
        }
        Ok(created_ids)
    })();

    let created_ids = match result {
        Ok(ids) => ids,
        Err(err) => {
            // Never leave a half-applied stitch behind.
            let _ = client.end_commit(session, CommitAction::Drop, "via-stitcher (failed)");
            return Err(err.context("placing vias; the commit was rolled back"));
        }
    };

    ipc::with_busy_retry("commit", || {
        client.end_commit(
            session.clone(),
            CommitAction::Commit,
            format!("via-stitcher: stitch {}", config.net),
        )
    })?;
    println!("committed {} via(s)", created_ids.len());

    // The ids echoed back by create_items have been seen to under-count what
    // actually landed, which silently orphans vias that `clear` then cannot
    // find. Compare against the board itself and say so rather than trusting
    // the response.
    let placed_on_board = count_vias(client)?.saturating_sub(vias_before);
    if placed_on_board != created_ids.len() {
        println!(
            "warning: the board gained {placed_on_board} via(s) but KiCad reported {} id(s).\n\
             {} via(s) are not tracked and `clear` will not remove them.",
            created_ids.len(),
            placed_on_board.saturating_sub(created_ids.len())
        );
    }

    // A second run adds to the field rather than replacing it, so its vias must
    // join the existing group. Creating another group of the same name would
    // leave `clear` looking at whichever it found first and missing the rest.
    let prior_ids: Vec<String> = if opts.replace {
        Vec::new()
    } else {
        existing.iter().map(|v| v.id.clone()).collect()
    };
    let all_ids = merge_group_members(&prior_ids, &created_ids);
    let group_id = upsert_group(client, &config.group_name, &config.net, &all_ids);

    match &group_id {
        Some(id) => println!("grouped as {:?} ({id})", config.group_name),
        None => println!(
            "note: KiCad would not create the group, so `clear` will rely on the manifest"
        ),
    }

    write_manifest(client, config, &all_ids, group_id.unwrap_or_default())?;

    println!("refilling zones...");
    ipc::with_busy_retry("refill zones", || client.refill_all_zones())?;

    if opts.save {
        ipc::with_busy_retry("save", || client.save_document())?;
        println!("board saved");
    } else {
        println!("board left unsaved — review it, then save or undo in KiCad");
    }
    Ok(())
}

/// Existing members first, then new ones, without duplicates. Order is stable
/// so a manifest diff between runs stays readable.
fn merge_group_members(prior: &[String], created: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(prior.len() + created.len());
    for id in prior.iter().chain(created.iter()) {
        if seen.insert(id.clone()) {
            out.push(id.clone());
        }
    }
    out
}

/// Groups the vias, in a commit of its own.
///
/// A group derives its layer set from its members, and KiCad resolves member
/// KIIDs against the *committed* board. Creating the group in the same commit
/// as the vias therefore yields a memberless group and is rejected with
/// "no overlapping layers with the board" — so the vias must be committed
/// first. The cost is a second undo entry.
///
/// The manifest covers `clear` if this fails, so a failure is reported rather
/// than fatal.
fn upsert_group(
    client: &KiCadClientBlocking,
    name: &str,
    net: &str,
    via_ids: &[String],
) -> Option<String> {
    if via_ids.is_empty() {
        return None;
    }
    let existing_id = crate::commands::clear::find_group(client, name)
        .ok()
        .flatten()
        .map(|(id, _)| id)
        .filter(|id| !id.is_empty());

    let group = kp::Group {
        id: existing_id.clone().map(|value| kp::Kiid { value }),
        name: name.to_string(),
        items: via_ids
            .iter()
            .map(|id| kp::Kiid { value: id.clone() })
            .collect(),
    };
    let payload = vec![kp::pack_any(&group, kp::GROUP_TYPE_URL)];

    let session = client.begin_commit().ok()?;
    let created = if existing_id.is_some() {
        client.update_items(payload)
    } else {
        client.create_items(payload, None)
    };
    match created {
        Ok(items) => {
            let id = items
                .first()
                .and_then(|any| kp::unpack_any::<kp::Group>(any, kp::GROUP_TYPE_URL))
                .and_then(|g| g.id)
                .map(|k| k.value);
            if client
                .end_commit(
                    session,
                    CommitAction::Commit,
                    format!("via-stitcher: group {net} stitching vias"),
                )
                .is_err()
            {
                return None;
            }
            id.or(existing_id)
        }
        Err(err) => {
            println!("  group creation rejected by KiCad: {err}");
            let _ = client.end_commit(session, CommitAction::Drop, "via-stitcher group (failed)");
            None
        }
    }
}

fn write_manifest(
    client: &KiCadClientBlocking,
    config: &Config,
    via_ids: &[String],
    group_id: String,
) -> Result<()> {
    let Some(board_path) = board_path(client) else {
        println!("note: could not determine the board's path, so no manifest was written");
        return Ok(());
    };
    let path = manifest::path_for_board(&board_path, &config.group_name);
    let manifest = Manifest {
        net: config.net.clone(),
        group_name: config.group_name.clone(),
        group_id,
        placed_at: manifest::timestamp(),
        via_diameter_mm: config.via.diameter_mm,
        via_drill_mm: config.via.drill_mm,
        vias: via_ids.to_vec(),
    };
    manifest.save(&path)?;
    println!("manifest written to {}", path.display());
    Ok(())
}

pub fn board_path(client: &KiCadClientBlocking) -> Option<std::path::PathBuf> {
    let docs = client
        .get_open_documents(kicad_ipc_rs::DocumentType::Pcb)
        .ok()?;
    let doc = docs.first()?;
    let filename = doc.board_filename.as_ref()?;
    let project_dir = client.get_current_project_path().ok()?;
    let dir: &Path = if project_dir.is_dir() {
        project_dir.as_path()
    } else {
        project_dir.parent()?
    };
    Some(dir.join(filename))
}

fn through_via(
    net: &kp::Net,
    at: Pt,
    diameter_nm: i64,
    drill_nm: i64,
    unconnected_layer_removal: i32,
) -> kp::Via {
    kp::Via {
        id: None,
        position: Some(kp::Vector2 {
            x_nm: at.x_nm.round() as i64,
            y_nm: at.y_nm.round() as i64,
        }),
        pad_stack: Some(kp::PadStack {
            r#type: kp::PAD_STACK_TYPE_NORMAL,
            layers: vec![kp::BOARD_LAYER_F_CU, kp::BOARD_LAYER_B_CU],
            drill: Some(kp::DrillProperties {
                start_layer: kp::BOARD_LAYER_F_CU,
                end_layer: kp::BOARD_LAYER_B_CU,
                diameter: Some(kp::Vector2 {
                    x_nm: drill_nm,
                    y_nm: drill_nm,
                }),
                shape: kp::DRILL_SHAPE_CIRCLE,
            }),
            unconnected_layer_removal,
            copper_layers: vec![kp::PadStackLayer {
                layer: kp::BOARD_LAYER_F_CU,
                shape: kp::PAD_STACK_SHAPE_CIRCLE,
                size: Some(kp::Vector2 {
                    x_nm: diameter_nm,
                    y_nm: diameter_nm,
                }),
            }],
        }),
        locked: kp::LOCKED_STATE_UNLOCKED,
        net: Some(net.clone()),
        r#type: kp::VIA_TYPE_THROUGH,
    }
}

fn count_vias(client: &KiCadClientBlocking) -> Result<usize> {
    let vias = ipc::with_busy_retry("count vias", || {
        client.get_items_by_type_codes(vec![kicad_ipc_rs::PcbObjectTypeCode::new_via().code])
    })?;
    Ok(vias.len())
}

fn layer_name(id: i32) -> String {
    BoardLayerInfo::canonical_name_for_id(id).unwrap_or_else(|| format!("layer{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry;
    use std::collections::BTreeMap;

    const F_CU: i32 = kp::BOARD_LAYER_F_CU;
    const IN1_CU: i32 = 4;
    const B_CU: i32 = kp::BOARD_LAYER_B_CU;

    fn square_mm(x: f64, y: f64, size: f64) -> Vec<(f64, f64)> {
        let (x, y, s) = (mm_to_nm(x), mm_to_nm(y), mm_to_nm(size));
        vec![(x, y), (x + s, y), (x + s, y + s), (x, y + s)]
    }

    fn config() -> Config {
        Config::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("stitch.toml").as_path())
            .expect("bundled config")
    }

    fn snapshot(
        target: BTreeMap<i32, Region>,
        foreign_fill: BTreeMap<i32, Region>,
        foreign_copper: BTreeMap<i32, Region>,
    ) -> BoardSnapshot {
        BoardSnapshot {
            target_fill: target,
            all_target_zones_filled: true,
            target_zone_count: 1,
            foreign_fill,
            foreign_copper,
            via_keepouts: Region::empty(),
            holes: Vec::new(),
            vias: Vec::new(),
            board_outline: Region::empty(),
        }
    }

    /// The case that matters on a real board: GND pours on two layers of a
    /// six-layer stack, and the layers in between carry someone else's plane.
    #[test]
    fn connects_on_a_subset_of_the_stack() {
        let plane = geometry::normalize(vec![square_mm(0.0, 0.0, 50.0)]);
        let mut target = BTreeMap::new();
        target.insert(IN1_CU, plane.clone());
        target.insert(B_CU, plane);

        let connect: BTreeSet<i32> = [IN1_CU, B_CU].into_iter().collect();
        let snap = snapshot(target, BTreeMap::new(), BTreeMap::new());
        let (region, admissible) = build_regions(&config(), &snap, &connect).expect("regions");

        assert!(region.contains(Pt::new(mm_to_nm(25.0), mm_to_nm(25.0))));
        assert!(admissible.contains(Pt::new(mm_to_nm(25.0), mm_to_nm(25.0))));
        // F.Cu carries no GND and is simply not a connect layer, so it neither
        // contributes to nor blocks the region.
        assert!(!admissible.is_empty());
    }

    #[test]
    fn a_connect_layer_without_fill_is_an_error() {
        let mut target = BTreeMap::new();
        target.insert(B_CU, geometry::normalize(vec![square_mm(0.0, 0.0, 50.0)]));
        let connect: BTreeSet<i32> = [F_CU, B_CU].into_iter().collect();
        let snap = snapshot(target, BTreeMap::new(), BTreeMap::new());

        let err = build_regions(&config(), &snap, &connect).unwrap_err().to_string();
        assert!(err.contains("F.Cu"), "error should name the layer: {err}");
    }

    #[test]
    fn connect_region_is_the_overlap_not_the_union() {
        let mut target = BTreeMap::new();
        target.insert(IN1_CU, geometry::normalize(vec![square_mm(0.0, 0.0, 50.0)]));
        target.insert(B_CU, geometry::normalize(vec![square_mm(30.0, 0.0, 50.0)]));

        let connect: BTreeSet<i32> = [IN1_CU, B_CU].into_iter().collect();
        let snap = snapshot(target, BTreeMap::new(), BTreeMap::new());
        let (region, _) = build_regions(&config(), &snap, &connect).expect("regions");

        assert!(region.contains(Pt::new(mm_to_nm(40.0), mm_to_nm(25.0))), "overlap");
        assert!(!region.contains(Pt::new(mm_to_nm(10.0), mm_to_nm(25.0))), "only In1");
        assert!(!region.contains(Pt::new(mm_to_nm(70.0), mm_to_nm(25.0))), "only B.Cu");
    }

    /// Foreign copper on a traversed layer must carve a hole in the region even
    /// though the plane is continuous on both connect layers.
    #[test]
    fn foreign_copper_on_a_traversed_layer_blocks_placement() {
        let plane = geometry::normalize(vec![square_mm(0.0, 0.0, 50.0)]);
        let mut target = BTreeMap::new();
        target.insert(IN1_CU, plane.clone());
        target.insert(B_CU, plane);

        let mut foreign = BTreeMap::new();
        foreign.insert(F_CU, geometry::normalize(vec![square_mm(20.0, 20.0, 10.0)]));

        let connect: BTreeSet<i32> = [IN1_CU, B_CU].into_iter().collect();
        let snap = snapshot(target, BTreeMap::new(), foreign);
        let (_, admissible) = build_regions(&config(), &snap, &connect).expect("regions");

        assert!(!admissible.contains(Pt::new(mm_to_nm(25.0), mm_to_nm(25.0))), "on the obstacle");
        assert!(admissible.contains(Pt::new(mm_to_nm(5.0), mm_to_nm(5.0))), "well clear of it");
    }

    /// Clearance is applied once, not twice: a point just over the configured
    /// clearance away from foreign copper must still be usable.
    #[test]
    fn obstacle_clearance_is_not_double_counted() {
        let plane = geometry::normalize(vec![square_mm(0.0, 0.0, 50.0)]);
        let mut target = BTreeMap::new();
        target.insert(IN1_CU, plane.clone());
        target.insert(B_CU, plane);

        let mut foreign = BTreeMap::new();
        foreign.insert(F_CU, geometry::normalize(vec![square_mm(0.0, 0.0, 25.0)]));

        let cfg = config();
        let connect: BTreeSet<i32> = [IN1_CU, B_CU].into_iter().collect();
        let snap = snapshot(target, BTreeMap::new(), foreign);
        let (_, admissible) = build_regions(&cfg, &snap, &connect).expect("regions");

        // The obstacle's edge is at x = 25mm. A via centre needs its own radius
        // plus clearance beyond that, and no more.
        let keepout_mm = cfg.via.diameter_mm / 2.0 + cfg.clearance.copper_mm;
        let just_clear = mm_to_nm(25.0 + keepout_mm * 1.10);
        let y = mm_to_nm(40.0);
        assert!(
            admissible.contains(Pt::new(just_clear, y)),
            "a point {keepout_mm}mm clear of the obstacle was rejected — clearance applied twice?"
        );
    }
}

#[cfg(test)]
mod step_tests {
    use super::*;

    #[test]
    fn search_step_divides_the_radius_into_several_rings() {
        for radius_mm in [0.5, 1.0, 2.0, 5.0] {
            let step = search_step_nm(radius_mm);
            let rings = (mm_to_nm(radius_mm) / step).floor() as usize;
            assert!(
                rings >= 4,
                "radius {radius_mm}mm gave only {rings} ring(s) — the search would barely move"
            );
        }
    }

    #[test]
    fn search_step_stays_sane_for_a_tiny_radius() {
        assert!(search_step_nm(0.01) >= 50_000.0);
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    #[test]
    fn merge_keeps_prior_members_and_appends_new_ones() {
        let merged = merge_group_members(
            &["a".to_string(), "b".to_string()],
            &["c".to_string(), "d".to_string()],
        );
        assert_eq!(merged, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn merge_does_not_duplicate_an_id_present_in_both() {
        let merged = merge_group_members(
            &["a".to_string(), "b".to_string()],
            &["b".to_string(), "c".to_string()],
        );
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_of_a_replace_run_is_just_the_new_vias() {
        let merged = merge_group_members(&[], &["x".to_string(), "y".to_string()]);
        assert_eq!(merged, vec!["x", "y"]);
    }
}

#[cfg(test)]
mod setback_tests {
    use super::*;
    use crate::geometry;

    fn config() -> Config {
        Config::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("stitch.toml")
                .as_path(),
        )
        .expect("bundled config")
    }

    fn square_mm(x: f64, y: f64, size: f64) -> Vec<(f64, f64)> {
        let (x, y, s) = (mm_to_nm(x), mm_to_nm(y), mm_to_nm(size));
        vec![(x, y), (x + s, y), (x + s, y + s), (x, y + s)]
    }

    #[test]
    fn a_setback_keeps_grid_candidates_away_from_the_boundary() {
        let region = geometry::normalize(vec![square_mm(0.0, 0.0, 50.0)]);
        let setback_mm = 5.0;
        let inset = region.offset(-mm_to_nm(setback_mm));

        let all = place::grid_candidates(
            &region,
            mm_to_nm(2.54),
            mm_to_nm(2.54),
            0.0,
            0.0,
        );
        let held_back = place::grid_candidates(
            &inset,
            mm_to_nm(2.54),
            mm_to_nm(2.54),
            0.0,
            0.0,
        );

        assert!(
            held_back.len() < all.len(),
            "the setback should remove candidates near the edge"
        );
        for c in &held_back {
            let d = nm_to_mm(c.at.x_nm.min(c.at.y_nm));
            let far = nm_to_mm(mm_to_nm(50.0) - c.at.x_nm.max(c.at.y_nm));
            assert!(
                d >= setback_mm - 0.01 && far >= setback_mm - 0.01,
                "candidate at ({:.2}, {:.2}) mm is inside the setback",
                nm_to_mm(c.at.x_nm),
                nm_to_mm(c.at.y_nm)
            );
        }
    }

    #[test]
    fn zero_setback_is_disabled() {
        let mut grid = config().grid;
        grid.edge_distance_mm = 0.0;
        assert!(!grid.has_edge_setback());
        grid.edge_distance_mm = 2.0;
        assert!(grid.has_edge_setback());
        grid.enabled = false;
        assert!(!grid.has_edge_setback(), "a disabled grid needs no setback");
    }

    #[test]
    fn setback_reference_falls_back_to_the_ring_reference() {
        let mut grid = config().grid;
        grid.edge_reference = None;
        assert_eq!(
            grid.edge_reference_or(EdgeReference::Board),
            EdgeReference::Board
        );
        grid.edge_reference = Some(EdgeReference::Plane);
        assert_eq!(
            grid.edge_reference_or(EdgeReference::Board),
            EdgeReference::Plane
        );
    }

    #[test]
    fn board_outline_is_only_fetched_when_something_measures_from_it() {
        let mut config = config();

        config.edge.enabled = true;
        config.edge.reference = EdgeReference::Plane;
        config.grid.edge_distance_mm = 0.0;
        assert!(!needs_board_outline(&config), "nothing uses Edge.Cuts");

        config.edge.reference = EdgeReference::Board;
        assert!(needs_board_outline(&config), "the ring uses Edge.Cuts");

        config.edge.reference = EdgeReference::Plane;
        config.grid.enabled = true;
        config.grid.edge_distance_mm = 3.0;
        config.grid.edge_reference = Some(EdgeReference::Board);
        assert!(needs_board_outline(&config), "the grid setback uses Edge.Cuts");
    }
}
