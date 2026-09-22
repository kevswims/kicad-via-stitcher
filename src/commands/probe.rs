//! Read-mostly capability check against the running KiCad instance.
//!
//! Everything this writes happens inside a commit that is always dropped, so a
//! probe run never changes the board.

use anyhow::{Context, Result, bail};
use kicad_ipc_rs::{CommitAction, KiCadClientBlocking, PcbObjectTypeCode};

use crate::ipc;
use crate::kicad_proto as kp;

pub fn run() -> Result<()> {
    let client = ipc::connect()?;

    let version = client.get_version()?;
    println!("KiCad {}", version.full_version);

    if !client.has_open_board()? {
        bail!("no board is open in the PCB editor");
    }

    let nets = client.get_nets()?;
    println!("nets: {}", nets.len());

    let vias = ipc::with_busy_retry("get vias", || {
        client.get_items_by_type_codes(vec![PcbObjectTypeCode::new_via().code])
    })?;
    println!("vias on board: {}", vias.len());

    probe_zones(&client)?;
    probe_existing_groups(&client)?;
    probe_group_of_committed_item(&client)?;
    probe_group_creation(&client)?;

    Ok(())
}

fn probe_zones(client: &KiCadClientBlocking) -> Result<()> {
    let raw = ipc::with_busy_retry("get zones", || {
        client.get_items_raw_by_type_codes(vec![PcbObjectTypeCode::new_zone().code])
    })?;
    println!("\nzones: {} raw item(s)", raw.len());

    let mut decoded = 0usize;
    for any in &raw {
        let Some(zone) = kp::unpack_any::<kp::Zone>(any, kp::ZONE_TYPE_URL) else {
            println!("  ! failed to decode a zone ({})", any.type_url);
            continue;
        };
        decoded += 1;

        let kind = match zone.r#type {
            kp::ZONE_TYPE_COPPER => "copper",
            kp::ZONE_TYPE_RULE_AREA => "rule-area",
            other => {
                println!("  zone type {other} (not copper or rule area)");
                continue;
            }
        };

        let layers: Vec<String> = zone.layers.iter().map(|id| layer_name(*id)).collect();
        let net = zone
            .copper_settings
            .as_ref()
            .and_then(|c| c.net.as_ref())
            .map(|n| n.name.clone())
            .unwrap_or_default();
        print!(
            "  [{kind}] net={net:?} name={:?} layers={} filled={} outline_polys={}",
            zone.name,
            layers.join(","),
            zone.filled,
            zone.outline.as_ref().map_or(0, |p| p.polygons.len()),
        );

        // All five flags, so a rule area that decodes as entirely permissive
        // is visibly a decode problem rather than a quiet wrong answer.
        if let Some(ra) = zone.rule_area_settings {
            print!(
                " keepout[copper={} vias={} tracks={} pads={} footprints={}]",
                ra.keepout_copper,
                ra.keepout_vias,
                ra.keepout_tracks,
                ra.keepout_pads,
                ra.keepout_footprints
            );
        } else if zone.r#type == kp::ZONE_TYPE_RULE_AREA {
            print!(" (no rule-area settings decoded)");
        }
        println!();

        for fp in &zone.filled_polygons {
            let shapes = fp.shapes.as_ref();
            let polys = shapes.map_or(0, |s| s.polygons.len());
            let (pts, arcs, holes) = shapes.map_or((0, 0, 0), count_nodes);
            println!(
                "      fill on {}: {polys} polygon(s), {pts} point node(s), {arcs} arc node(s), {holes} hole(s)",
                layer_name(fp.layer),
            );
        }
    }

    println!("decoded {decoded}/{} zone(s)", raw.len());
    Ok(())
}

fn count_nodes(set: &kp::PolySet) -> (usize, usize, usize) {
    let (mut pts, mut arcs, mut holes) = (0, 0, 0);
    for poly in &set.polygons {
        holes += poly.holes.len();
        for line in poly.outline.iter().chain(poly.holes.iter()) {
            for node in &line.nodes {
                match node.geometry {
                    Some(kp::poly_line_node::Geometry::Point(_)) => pts += 1,
                    Some(kp::poly_line_node::Geometry::Arc(_)) => arcs += 1,
                    None => {}
                }
            }
        }
    }
    (pts, arcs, holes)
}

/// Decodes the board's existing groups with our own `Group` message. If real
/// groups round-trip, the field tags are right and any create failure is about
/// KiCad's handler rather than our encoding.
fn probe_existing_groups(client: &KiCadClientBlocking) -> Result<()> {
    println!("\n-- existing groups --");
    let raw = ipc::with_busy_retry("get groups (raw)", || {
        client.get_items_raw_by_type_codes(vec![PcbObjectTypeCode::new_group().code])
    })?;
    println!("  {} group(s) on the board", raw.len());
    for any in raw.iter().take(5) {
        match kp::unpack_any::<kp::Group>(any, kp::GROUP_TYPE_URL) {
            Some(g) => println!(
                "  decoded: name={:?} items={} id={:?}",
                g.name,
                g.items.len(),
                g.id.map(|k| k.value).unwrap_or_default()
            ),
            None => println!("  ! failed to decode ({})", any.type_url),
        }
    }
    Ok(())
}

/// Groups an item that is already committed to the board. If this succeeds
/// where grouping a freshly created via fails, the constraint is that member
/// KIIDs must already exist when the group is created — which means vias have
/// to be committed before the group referencing them.
fn probe_group_of_committed_item(client: &KiCadClientBlocking) -> Result<()> {
    println!("\n-- group over an already-committed via --");
    let vias = ipc::with_busy_retry("get vias", || {
        client.get_items_by_type_codes(vec![PcbObjectTypeCode::new_via().code])
    })?;
    let existing_id = vias.into_iter().find_map(|item| match item {
        kicad_ipc_rs::PcbItem::Via(v) => v.id,
        _ => None,
    });
    let Some(existing_id) = existing_id else {
        println!("  board has no vias to test with");
        return Ok(());
    };

    let group = kp::Group {
        id: None,
        name: "via-stitch:probe-existing".to_string(),
        items: vec![kp::Kiid {
            value: existing_id.clone(),
        }],
    };

    let session = client.begin_commit()?;
    match client.create_items(vec![kp::pack_any(&group, kp::GROUP_TYPE_URL)], None) {
        Ok(items) => {
            let id = items
                .first()
                .and_then(|any| kp::unpack_any::<kp::Group>(any, kp::GROUP_TYPE_URL))
                .and_then(|g| g.id)
                .map(|k| k.value)
                .unwrap_or_default();
            println!("  WORKS over committed via {existing_id} (group id={id:?})");
        }
        Err(err) => println!("  FAILED over committed via {existing_id}: {err}"),
    }
    client.end_commit(session, CommitAction::Drop, "via-stitcher probe")?;
    println!("  commit dropped, board unchanged");
    Ok(())
}

/// Confirms the ordering constraint the tool relies on: a group created in the
/// same commit as its members is rejected, because KiCad resolves member KIIDs
/// against the committed board and the group ends up with no layers.
fn probe_group_creation(client: &KiCadClientBlocking) -> Result<()> {
    println!("\n-- group over a via in the same commit --");

    let net = kp::Net {
        code: None,
        name: first_net_name(client)?,
    };
    let via = through_via(&net, 100_000_000, 100_000_000, 600_000, 300_000);

    let session = client.begin_commit()?;
    let outcome = (|| -> Result<String> {
        let created = client.create_items(vec![kp::pack_any(&via, kp::VIA_TYPE_URL)], None)?;
        let via_id = created
            .first()
            .and_then(|any| kp::unpack_any::<kp::Via>(any, kp::VIA_TYPE_URL))
            .and_then(|v| v.id)
            .map(|k| k.value)
            .context("create_items returned no usable via id")?;

        let group = kp::Group {
            id: None,
            name: "via-stitch:probe".to_string(),
            items: vec![kp::Kiid { value: via_id }],
        };
        match client.create_items(vec![kp::pack_any(&group, kp::GROUP_TYPE_URL)], None) {
            Ok(_) => Ok("unexpectedly succeeded — the two-commit split may no longer be needed"
                .to_string()),
            Err(err) => Ok(format!("rejected as expected: {err}")),
        }
    })();

    client.end_commit(session, CommitAction::Drop, "via-stitcher probe")?;
    println!("  {}", outcome?);
    println!("  commit dropped, board unchanged");
    println!("\n=> stitch commits vias first, then groups them in a second commit");
    Ok(())
}

fn first_net_name(client: &KiCadClientBlocking) -> Result<String> {
    let nets = client.get_nets()?;
    nets.iter()
        .find(|n| !n.name.is_empty() && n.name != "unconnected")
        .map(|n| n.name.clone())
        .context("board has no named nets to probe with")
}

fn through_via(net: &kp::Net, x_nm: i64, y_nm: i64, diameter_nm: i64, drill_nm: i64) -> kp::Via {
    kp::Via {
        id: None,
        position: Some(kp::Vector2 { x_nm, y_nm }),
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
            unconnected_layer_removal: kp::ULR_KEEP,
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

fn layer_name(id: i32) -> String {
    kicad_ipc_rs::BoardLayerInfo::canonical_name_for_id(id).unwrap_or_else(|| format!("layer{id}"))
}
