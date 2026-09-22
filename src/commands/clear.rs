//! `clear` — remove the vias a previous `stitch` run placed.

use anyhow::{Result, bail};
use kicad_ipc_rs::{CommitAction, KiCadClientBlocking, PcbItem, PcbObjectTypeCode};
use std::collections::BTreeSet;

use crate::config::Config;
use crate::ipc;
use crate::manifest::{self, Manifest};

pub struct Options {
    pub config_path: std::path::PathBuf,
    pub save: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let config = Config::load(&opts.config_path)?;
    let client = ipc::connect()?;

    if !ipc::with_busy_retry("check for an open board", || client.has_open_board())? {
        bail!("no board is open in the PCB editor");
    }

    let group = find_group(&client, &config.group_name)?;
    let ids = stitch_via_ids(&client, &config)?;

    if ids.is_empty() {
        // An emptied group can outlive its members, and leaving it would make
        // the next stitch create a second group of the same name.
        match group.as_ref().map(|(id, _)| id).filter(|id| !id.is_empty()) {
            Some(group_id) => {
                let session = ipc::with_busy_retry("begin commit", || client.begin_commit())?;
                if let Err(err) = client.delete_items(vec![group_id.clone()]) {
                    let _ =
                        client.end_commit(session, CommitAction::Drop, "via-stitcher clear (failed)");
                    return Err(anyhow::Error::new(err).context("removing the empty group"));
                }
                ipc::with_busy_retry("commit", || {
                    client.end_commit(
                        session.clone(),
                        CommitAction::Commit,
                        format!("via-stitcher: clear {}", config.net),
                    )
                })?;
                println!("no stitching vias left; removed the empty group {:?}", config.group_name);
            }
            None => println!("no stitching vias found for {:?}", config.group_name),
        }
        return Ok(());
    }

    // Only delete ids the board still has. A via removed by hand should not
    // make the whole run look like a failure.
    let present = present_via_ids(&client)?;
    let (live, missing): (Vec<String>, Vec<String>) =
        ids.into_iter().partition(|id| present.contains(id));

    if !missing.is_empty() {
        println!(
            "{} recorded via(s) are no longer on the board and will be ignored",
            missing.len()
        );
    }
    if live.is_empty() {
        println!("nothing left to delete");
        return Ok(());
    }

    // The group goes with its members, in the same commit.
    let group_id = group.map(|(id, _)| id);
    let mut to_delete = live.clone();
    if let Some(id) = group_id.as_ref().filter(|id| !id.is_empty()) {
        to_delete.push(id.clone());
    }

    let session = ipc::with_busy_retry("begin commit", || client.begin_commit())?;
    if let Err(err) = client.delete_items(to_delete) {
        let _ = client.end_commit(session, CommitAction::Drop, "via-stitcher clear (failed)");
        return Err(anyhow::Error::new(err).context("deleting stitching vias; rolled back"));
    }
    ipc::with_busy_retry("commit", || {
        client.end_commit(
            session.clone(),
            CommitAction::Commit,
            format!("via-stitcher: clear {}", config.net),
        )
    })?;

    // KiCad's delete response can omit per-item rows, so its own count is not
    // trustworthy. Re-read the board and report what actually went.
    let remaining = present_via_ids(&client)?;
    let still_there = live.iter().filter(|id| remaining.contains(*id)).count();
    println!("deleted {} via(s)", live.len() - still_there);
    if still_there > 0 {
        println!("warning: {still_there} via(s) could not be deleted and are still on the board");
    }
    if group_id.is_some() {
        println!("removed group {:?}", config.group_name);
    }

    if let Some(path) = crate::commands::stitch::board_path(&client) {
        let manifest_path = manifest::path_for_board(&path, &config.group_name);
        if manifest_path.exists() {
            std::fs::remove_file(&manifest_path).ok();
            println!("removed {}", manifest_path.display());
        }
    }

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

/// Every via this tool is responsible for, as the union of the board's group
/// and the sidecar manifest.
///
/// Neither source alone is sufficient. The group tracks edits made in the GUI
/// but has been observed to come back one member short of what was placed, and
/// an emptied group can outlive its membership entirely; the manifest is an
/// exact record of one run but goes stale if vias are deleted by hand. Taking
/// the union means a via has to be missing from *both* to be left behind, and
/// ids that no longer exist are filtered out by the caller anyway.
pub fn stitch_via_ids(
    client: &KiCadClientBlocking,
    config: &Config,
) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();

    if let Some((_, members)) = find_group(client, &config.group_name)? {
        ids.extend(members);
    }

    if let Some(board) = crate::commands::stitch::board_path(client) {
        let path = manifest::path_for_board(&board, &config.group_name);
        if let Some(found) = Manifest::load(&path)? {
            if found.group_name == config.group_name {
                ids.extend(found.vias);
            }
        }
    }

    Ok(ids)
}

/// The group's own id alongside its members. The id matters because an emptied
/// group must be removed too: leaving it behind would make the next `stitch`
/// create a second group of the same name, and `clear` would then find the
/// stale empty one and report nothing to delete.
pub fn find_group(
    client: &KiCadClientBlocking,
    group_name: &str,
) -> Result<Option<(String, BTreeSet<String>)>> {
    let groups = ipc::with_busy_retry("get groups", || {
        client.get_items_by_type_codes(vec![PcbObjectTypeCode::new_group().code])
    })?;
    for item in groups {
        let PcbItem::Group(group) = item else { continue };
        if group.name == group_name {
            let id = group.id.unwrap_or_default();
            return Ok(Some((id, group.item_ids.into_iter().collect())));
        }
    }
    Ok(None)
}

fn present_via_ids(client: &KiCadClientBlocking) -> Result<BTreeSet<String>> {
    let vias = ipc::with_busy_retry("get vias", || {
        client.get_items_by_type_codes(vec![PcbObjectTypeCode::new_via().code])
    })?;
    Ok(vias
        .into_iter()
        .filter_map(|item| match item {
            PcbItem::Via(v) => v.id,
            _ => None,
        })
        .collect())
}
