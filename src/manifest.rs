//! Sidecar record of what a `stitch` run placed.
//!
//! The named PCB group is the primary handle on those vias, but the group is
//! only as durable as KiCad's support for creating one. The manifest is the
//! fallback: it records the KIIDs directly, so `clear` still works if the group
//! never materialised or was dissolved in the GUI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub net: String,
    pub group_name: String,
    /// Empty when group creation was not available.
    #[serde(default)]
    pub group_id: String,
    pub placed_at: String,
    pub via_diameter_mm: f64,
    pub via_drill_mm: f64,
    pub vias: Vec<String>,
}

/// `<board-stem>.<group>.stitch-manifest.toml`, alongside the board.
///
/// The group name is part of the filename so several configs can target one
/// board — an interior-grid run and a perimeter run, say — without their
/// manifests overwriting each other.
pub fn path_for_board(board_path: &Path, group_name: &str) -> PathBuf {
    let stem = board_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "board".to_string());
    board_path.with_file_name(format!("{stem}.{}.stitch-manifest.toml", slug(group_name)))
}

/// Reduces a group name to something safe in a filename.
fn slug(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "stitch".to_string()
    } else {
        trimmed
    }
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        Ok(Some(manifest))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing manifest")?;
        std::fs::write(path, text).with_context(|| format!("writing manifest {}", path.display()))
    }
}

pub fn timestamp() -> String {
    // Seconds since the epoch keeps this dependency-free; the manifest only
    // needs enough to tell two runs apart.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_toml() {
        let manifest = Manifest {
            net: "GND".into(),
            group_name: "via-stitch:GND".into(),
            group_id: "b236ca01-747a-4848-8f82-4e813a1d2236".into(),
            placed_at: "1758456000".into(),
            via_diameter_mm: 0.6,
            via_drill_mm: 0.3,
            vias: vec!["0f082db0-aaaa".into(), "110e4871-bbbb".into()],
        };
        let text = toml::to_string_pretty(&manifest).expect("serialize");
        let back: Manifest = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.vias, manifest.vias);
        assert_eq!(back.group_id, manifest.group_id);
        assert_eq!(back.via_drill_mm, manifest.via_drill_mm);
    }

    #[test]
    fn manifest_sits_next_to_the_board() {
        let p = path_for_board(
            Path::new("/tmp/proj/next-device.kicad_pcb"),
            "via-stitch:GND",
        );
        assert_eq!(
            p,
            Path::new("/tmp/proj/next-device.via-stitch-GND.stitch-manifest.toml")
        );
    }

    #[test]
    fn different_groups_get_different_manifests() {
        let board = Path::new("/tmp/proj/next-device.kicad_pcb");
        assert_ne!(
            path_for_board(board, "via-stitch:GND-grid"),
            path_for_board(board, "via-stitch:GND-edge")
        );
    }

    #[test]
    fn a_degenerate_group_name_still_yields_a_filename() {
        let p = path_for_board(Path::new("/tmp/b.kicad_pcb"), ":::");
        assert_eq!(p, Path::new("/tmp/b.stitch.stitch-manifest.toml"));
    }

    #[test]
    fn a_missing_manifest_is_not_an_error() {
        let p = Path::new("/tmp/definitely-not-here-12345.stitch-manifest.toml");
        assert!(Manifest::load(p).expect("load").is_none());
    }
}
