//! Minimal wire-compatible subset of KiCad's IPC board-item protobuf schema
//! (`kiapi.board.types` / `kiapi.common.types`).
//!
//! `kicad-ipc-rs` vendors the full generated schema but keeps it `pub(crate)`,
//! so the message types can't be imported. These structs carry only the fields
//! we set or read; protobuf is sparse by design, so omitted fields are
//! indistinguishable on the wire from a message that set them to their default.
//! Field tags and enum discriminants were read from `kicad-ipc-rs`'s own
//! vendored generated code, which is the exact schema this KiCad instance
//! already round-trips through other `kicad-ipc-rs` calls.
//!
//! Enum fields are declared as plain `int32` (protobuf enums are int32 on the
//! wire) rather than as real Rust enums, using the raw discriminants documented
//! alongside each constant.

use prost::Message;

pub const VIA_TYPE_URL: &str = "type.googleapis.com/kiapi.board.types.Via";
pub const ZONE_TYPE_URL: &str = "type.googleapis.com/kiapi.board.types.Zone";
pub const GROUP_TYPE_URL: &str = "type.googleapis.com/kiapi.board.types.Group";

/// `BoardLayer::BL_F_Cu`. Note this is the IPC enum, which differs from the
/// `.kicad_pcb` file format's layer numbering (where F.Cu is 0 and B.Cu is 2).
pub const BOARD_LAYER_F_CU: i32 = 3;
/// `BoardLayer::BL_B_Cu`
pub const BOARD_LAYER_B_CU: i32 = 34;

/// `LockedState::LS_UNLOCKED`
pub const LOCKED_STATE_UNLOCKED: i32 = 1;

/// `PadStackType::PST_NORMAL` (same shape on every copper layer). Valid here
/// because we only emit through vias, which always span F.Cu. KiCad rejects
/// `PST_NORMAL` ("could not unpack PCB_VIA") for any span that does not include
/// F.Cu specifically — including a blind via between B.Cu and an inner layer —
/// so extending this to blind/buried vias requires `PST_CUSTOM` (=3) instead.
pub const PAD_STACK_TYPE_NORMAL: i32 = 1;
/// `PadStackShape::PSS_CIRCLE`
pub const PAD_STACK_SHAPE_CIRCLE: i32 = 1;
/// `DrillShape::DS_CIRCLE`
pub const DRILL_SHAPE_CIRCLE: i32 = 1;
/// `ViaType::VT_THROUGH` (spans the board's two outermost copper layers).
pub const VIA_TYPE_THROUGH: i32 = 1;

/// `UnconnectedLayerRemoval` — controls which annular rings survive on layers
/// the via passes through without connecting to.
pub const ULR_KEEP: i32 = 1;
pub const ULR_REMOVE: i32 = 2;
pub const ULR_REMOVE_EXCEPT_START_AND_END: i32 = 3;
pub const ULR_START_END_ONLY: i32 = 4;

/// `ZoneType`
pub const ZONE_TYPE_COPPER: i32 = 1;
pub const ZONE_TYPE_RULE_AREA: i32 = 3;

#[derive(Clone, PartialEq, Message)]
pub struct Kiid {
    #[prost(string, tag = "1")]
    pub value: String,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct Vector2 {
    #[prost(int64, tag = "1")]
    pub x_nm: i64,
    #[prost(int64, tag = "2")]
    pub y_nm: i64,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct NetCode {
    #[prost(int32, tag = "1")]
    pub value: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Net {
    #[prost(message, optional, tag = "1")]
    pub code: Option<NetCode>,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct DrillProperties {
    #[prost(int32, tag = "1")]
    pub start_layer: i32,
    #[prost(int32, tag = "2")]
    pub end_layer: i32,
    #[prost(message, optional, tag = "3")]
    pub diameter: Option<Vector2>,
    #[prost(int32, tag = "4")]
    pub shape: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct PadStackLayer {
    #[prost(int32, tag = "1")]
    pub layer: i32,
    #[prost(int32, tag = "2")]
    pub shape: i32,
    #[prost(message, optional, tag = "3")]
    pub size: Option<Vector2>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PadStack {
    #[prost(int32, tag = "1")]
    pub r#type: i32,
    #[prost(int32, repeated, tag = "2")]
    pub layers: Vec<i32>,
    #[prost(message, optional, tag = "3")]
    pub drill: Option<DrillProperties>,
    #[prost(int32, tag = "4")]
    pub unconnected_layer_removal: i32,
    #[prost(message, repeated, tag = "5")]
    pub copper_layers: Vec<PadStackLayer>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Via {
    #[prost(message, optional, tag = "1")]
    pub id: Option<Kiid>,
    #[prost(message, optional, tag = "2")]
    pub position: Option<Vector2>,
    #[prost(message, optional, tag = "3")]
    pub pad_stack: Option<PadStack>,
    #[prost(int32, tag = "4")]
    pub locked: i32,
    #[prost(message, optional, tag = "5")]
    pub net: Option<Net>,
    #[prost(int32, tag = "6")]
    pub r#type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Group {
    #[prost(message, optional, tag = "1")]
    pub id: Option<Kiid>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(message, repeated, tag = "3")]
    pub items: Vec<Kiid>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct ArcStartMidEnd {
    #[prost(message, optional, tag = "1")]
    pub start: Option<Vector2>,
    #[prost(message, optional, tag = "2")]
    pub mid: Option<Vector2>,
    #[prost(message, optional, tag = "3")]
    pub end: Option<Vector2>,
}

/// A polyline vertex is a oneof: either a plain point or an arc segment.
/// Zone outlines really do carry arcs, so decoders must handle both.
pub mod poly_line_node {
    #[derive(Clone, Copy, PartialEq, ::prost::Oneof)]
    pub enum Geometry {
        #[prost(message, tag = "1")]
        Point(super::Vector2),
        #[prost(message, tag = "2")]
        Arc(super::ArcStartMidEnd),
    }
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct PolyLineNode {
    #[prost(oneof = "poly_line_node::Geometry", tags = "1, 2")]
    pub geometry: Option<poly_line_node::Geometry>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PolyLine {
    #[prost(message, repeated, tag = "1")]
    pub nodes: Vec<PolyLineNode>,
    #[prost(bool, tag = "2")]
    pub closed: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct PolygonWithHoles {
    #[prost(message, optional, tag = "1")]
    pub outline: Option<PolyLine>,
    #[prost(message, repeated, tag = "2")]
    pub holes: Vec<PolyLine>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PolySet {
    #[prost(message, repeated, tag = "1")]
    pub polygons: Vec<PolygonWithHoles>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ZoneFilledPolygons {
    #[prost(int32, tag = "1")]
    pub layer: i32,
    #[prost(message, optional, tag = "2")]
    pub shapes: Option<PolySet>,
}

/// Only the net is decoded. In the real schema this and `RuleAreaSettings`
/// are a oneof (tags 6 and 7); declaring them as two optional fields decodes
/// identically, since at most one is ever present on the wire.
#[derive(Clone, PartialEq, Message)]
pub struct CopperZoneSettings {
    #[prost(message, optional, tag = "8")]
    pub net: Option<Net>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct RuleAreaSettings {
    #[prost(bool, tag = "1")]
    pub keepout_copper: bool,
    #[prost(bool, tag = "2")]
    pub keepout_vias: bool,
    #[prost(bool, tag = "3")]
    pub keepout_tracks: bool,
    #[prost(bool, tag = "4")]
    pub keepout_pads: bool,
    #[prost(bool, tag = "5")]
    pub keepout_footprints: bool,
}

/// Only the fields we read.
#[derive(Clone, PartialEq, Message)]
pub struct Zone {
    #[prost(message, optional, tag = "1")]
    pub id: Option<Kiid>,
    #[prost(int32, tag = "2")]
    pub r#type: i32,
    #[prost(int32, repeated, tag = "3")]
    pub layers: Vec<i32>,
    #[prost(message, optional, tag = "4")]
    pub outline: Option<PolySet>,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(message, optional, tag = "6")]
    pub copper_settings: Option<CopperZoneSettings>,
    #[prost(message, optional, tag = "7")]
    pub rule_area_settings: Option<RuleAreaSettings>,
    #[prost(uint32, tag = "8")]
    pub priority: u32,
    #[prost(bool, tag = "9")]
    pub filled: bool,
    #[prost(message, repeated, tag = "10")]
    pub filled_polygons: Vec<ZoneFilledPolygons>,
}

pub fn pack_any<T: Message>(message: &T, type_url: &str) -> prost_types::Any {
    prost_types::Any {
        type_url: type_url.to_string(),
        value: message.encode_to_vec(),
    }
}

/// Decodes a `prost_types::Any` whose `type_url` matches `type_url`.
pub fn unpack_any<T: Message + Default>(any: &prost_types::Any, type_url: &str) -> Option<T> {
    // KiCad sends fully-qualified `type.googleapis.com/...` URLs, but compare
    // on the trailing message name so a host-prefix change doesn't break us.
    let want = type_url.rsplit('/').next()?;
    let got = any.type_url.rsplit('/').next()?;
    if want != got {
        return None;
    }
    T::decode(any.value.as_slice()).ok()
}
