//! Stitching parameters, loaded from TOML.
//!
//! Distances are millimetres here and converted to KiCad's integer nanometres
//! once, at the IPC boundary. Checks that need the board (does the net exist,
//! which layers actually carry a zone) happen after connecting, not in
//! `validate`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub net: String,
    pub group_name: String,
    pub connect: Connect,
    pub via: Via,
    #[serde(default)]
    pub traversal: Traversal,
    pub clearance: Clearance,
    pub grid: Grid,
    pub edge: Edge,
    pub search: Search,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connect {
    pub layers: ConnectLayers,
}

/// `"auto"` resolves, once the board is read, to every copper layer carrying a
/// zone on the target net — which on a real board is usually a subset of the
/// copper stack, since the other layers hold power planes the via only passes
/// through.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ConnectLayers {
    Auto(AutoKeyword),
    Explicit(Vec<String>),
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoKeyword {
    Auto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Via {
    pub diameter_mm: f64,
    pub drill_mm: f64,
    #[serde(default)]
    pub unconnected_layer_removal: UnconnectedLayerRemoval,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnconnectedLayerRemoval {
    Keep,
    Remove,
    /// Strips annular rings from the inner planes the via merely passes
    /// through while keeping the outer-layer rings.
    #[default]
    RemoveExceptEnds,
    EndsOnly,
}

impl UnconnectedLayerRemoval {
    pub fn to_proto(self) -> i32 {
        use crate::kicad_proto as kp;
        match self {
            Self::Keep => kp::ULR_KEEP,
            Self::Remove => kp::ULR_REMOVE,
            Self::RemoveExceptEnds => kp::ULR_REMOVE_EXCEPT_START_AND_END,
            Self::EndsOnly => kp::ULR_START_END_ONLY,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Traversal {
    #[serde(default)]
    pub default: TraversalPolicy,
    #[serde(default)]
    pub layers: BTreeMap<String, TraversalPolicy>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraversalPolicy {
    /// Foreign zone fill is not an obstacle; KiCad's refill voids around the
    /// new via. Foreign tracks and pads are still obstacles — a refill does
    /// not carve around those.
    #[default]
    Allow,
    /// Foreign zone fill blocks placement, preserving the plane intact.
    Avoid,
}

impl Traversal {
    pub fn policy_for(&self, layer_name: &str) -> TraversalPolicy {
        self.layers.get(layer_name).copied().unwrap_or(self.default)
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clearance {
    pub copper_mm: f64,
    pub drill_to_drill_mm: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grid {
    pub enabled: bool,
    pub x_spacing_mm: f64,
    pub y_spacing_mm: f64,
    pub origin_x_mm: f64,
    pub origin_y_mm: f64,
    /// Keep grid vias at least this far inside the edge, so a loose interior
    /// grid does not crowd a tightly-spaced perimeter ring. 0 disables the
    /// setback and lets the grid run right up to the plane boundary.
    #[serde(default)]
    pub edge_distance_mm: f64,
    /// What that distance is measured from. Defaults to whatever the perimeter
    /// ring follows, since staying clear of the ring is the point.
    #[serde(default)]
    pub edge_reference: Option<EdgeReference>,
}

impl Grid {
    /// The setback reference, falling back to the edge ring's own reference.
    pub fn edge_reference_or(&self, fallback: EdgeReference) -> EdgeReference {
        self.edge_reference.unwrap_or(fallback)
    }

    pub fn has_edge_setback(&self) -> bool {
        self.enabled && self.edge_distance_mm > 0.0
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub enabled: bool,
    pub reference: EdgeReference,
    pub distance_mm: f64,
    pub spacing_mm: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeReference {
    /// The outline of the region where the plane is present on every connect
    /// layer, which follows internal cutouts and plane islands.
    Plane,
    /// The board outline from Edge.Cuts.
    Board,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub radius_mm: f64,
    pub min_spacing_mm: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |msg: String| Err(ConfigError::Invalid(msg));

        if self.net.trim().is_empty() {
            return bad("`net` must not be empty".into());
        }
        if self.group_name.trim().is_empty() {
            return bad("`group_name` must not be empty".into());
        }
        if let ConnectLayers::Explicit(layers) = &self.connect.layers {
            if layers.len() < 2 {
                return bad(format!(
                    "`connect.layers` needs at least 2 layers to stitch between, got {}",
                    layers.len()
                ));
            }
            let mut seen = layers.clone();
            seen.sort();
            seen.dedup();
            if seen.len() != layers.len() {
                return bad("`connect.layers` contains duplicates".into());
            }
        }
        if self.via.drill_mm <= 0.0 || self.via.diameter_mm <= 0.0 {
            return bad("`via.diameter_mm` and `via.drill_mm` must be positive".into());
        }
        if self.via.drill_mm >= self.via.diameter_mm {
            return bad(format!(
                "`via.drill_mm` ({}) must be smaller than `via.diameter_mm` ({}) — a via needs an annular ring",
                self.via.drill_mm, self.via.diameter_mm
            ));
        }
        if self.clearance.copper_mm < 0.0 || self.clearance.drill_to_drill_mm < 0.0 {
            return bad("clearances must not be negative".into());
        }
        if !self.grid.enabled && !self.edge.enabled {
            return bad("both `grid.enabled` and `edge.enabled` are false — nothing to do".into());
        }
        if self.grid.enabled && (self.grid.x_spacing_mm <= 0.0 || self.grid.y_spacing_mm <= 0.0) {
            return bad("`grid` spacings must be positive".into());
        }
        if self.edge.enabled && self.edge.spacing_mm <= 0.0 {
            return bad("`edge.spacing_mm` must be positive".into());
        }
        if self.edge.enabled && self.edge.distance_mm < 0.0 {
            return bad("`edge.distance_mm` must not be negative".into());
        }
        if self.grid.edge_distance_mm < 0.0 {
            return bad("`grid.edge_distance_mm` must not be negative".into());
        }
        if self.search.radius_mm < 0.0 {
            return bad("`search.radius_mm` must not be negative".into());
        }
        if self.grid.enabled {
            let tightest = self.grid.x_spacing_mm.min(self.grid.y_spacing_mm);
            if self.search.min_spacing_mm > tightest {
                return bad(format!(
                    "`search.min_spacing_mm` ({}) exceeds the tightest grid spacing ({tightest}) — the grid would reject itself",
                    self.search.min_spacing_mm
                ));
            }
        }
        Ok(())
    }

    pub fn connect_layers(&self) -> Option<&[String]> {
        match &self.connect.layers {
            ConnectLayers::Auto(_) => None,
            ConnectLayers::Explicit(layers) => Some(layers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("stitch.toml");
        Config::load(&path).expect("bundled stitch.toml should load")
    }

    #[test]
    fn bundled_config_loads_and_validates() {
        let config = sample();
        assert_eq!(config.net, "GND");
        assert!(matches!(config.connect.layers, ConnectLayers::Auto(_)));
    }

    #[test]
    fn traversal_uses_the_per_layer_override_then_the_default() {
        let traversal: Traversal = toml::from_str(
            r#"
            default = "allow"
            [layers]
            "In3.Cu" = "avoid"
            "#,
        )
        .expect("traversal table should parse");

        assert_eq!(traversal.policy_for("In3.Cu"), TraversalPolicy::Avoid);
        assert_eq!(traversal.policy_for("In1.Cu"), TraversalPolicy::Allow);
        assert_eq!(traversal.policy_for("anything else"), TraversalPolicy::Allow);
    }

    #[test]
    fn bundled_traversal_only_names_real_copper_layers() {
        let config = sample();
        for name in config.traversal.layers.keys() {
            assert!(
                kicad_ipc_rs::BoardLayerInfo::id_from_name(name).is_some(),
                "{name} in [traversal.layers] is not a layer KiCad knows"
            );
        }
    }

    #[test]
    fn rejects_drill_wider_than_pad() {
        let mut config = sample();
        config.via.drill_mm = config.via.diameter_mm;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_min_spacing_wider_than_grid() {
        let mut config = sample();
        config.search.min_spacing_mm = config.grid.x_spacing_mm * 2.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_single_connect_layer() {
        let mut config = sample();
        config.connect.layers = ConnectLayers::Explicit(vec!["F.Cu".into()]);
        assert!(config.validate().is_err());
    }
}
