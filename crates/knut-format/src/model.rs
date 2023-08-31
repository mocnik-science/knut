//! Logical data model shared by builder and reader.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub type Str = Arc<str>;
pub type Tags = Arc<[(Str, Str)]>;

/// Relative time value meaning "open ended".
pub const T_OPEN: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum ObjType {
    Node = 0,
    Way = 1,
    Relation = 2,
}

impl ObjType {
    pub const ALL: [ObjType; 3] = [ObjType::Node, ObjType::Way, ObjType::Relation];

    pub fn from_u8(v: u8) -> Option<ObjType> {
        match v {
            0 => Some(ObjType::Node),
            1 => Some(ObjType::Way),
            2 => Some(ObjType::Relation),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ObjType::Node => "node",
            ObjType::Way => "way",
            ObjType::Relation => "relation",
        }
    }

    pub fn parse(s: &str) -> Option<ObjType> {
        match s {
            "n" | "node" => Some(ObjType::Node),
            "w" | "way" => Some(ObjType::Way),
            "r" | "rel" | "relation" => Some(ObjType::Relation),
            _ => None,
        }
    }
}

/// Axis aligned box in 1e-7 degrees. Empty if `minx > maxx`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BBox {
    pub minx: i32,
    pub miny: i32,
    pub maxx: i32,
    pub maxy: i32,
}

impl Default for BBox {
    fn default() -> Self {
        BBox::EMPTY
    }
}

impl BBox {
    pub const EMPTY: BBox = BBox { minx: i32::MAX, miny: i32::MAX, maxx: i32::MIN, maxy: i32::MIN };
    pub const WORLD: BBox =
        BBox { minx: -1_800_000_000, miny: -900_000_000, maxx: 1_800_000_000, maxy: 900_000_000 };

    pub fn new(minx: i32, miny: i32, maxx: i32, maxy: i32) -> BBox {
        BBox { minx, miny, maxx, maxy }
    }

    pub fn point(x: i32, y: i32) -> BBox {
        BBox { minx: x, miny: y, maxx: x, maxy: y }
    }

    /// Builds a box from degrees.
    pub fn from_deg(minlon: f64, minlat: f64, maxlon: f64, maxlat: f64) -> BBox {
        BBox {
            minx: deg_to_e7(minlon),
            miny: deg_to_e7(minlat),
            maxx: deg_to_e7(maxlon),
            maxy: deg_to_e7(maxlat),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.minx > self.maxx || self.miny > self.maxy
    }

    #[inline]
    pub fn extend(&mut self, x: i32, y: i32) {
        self.minx = self.minx.min(x);
        self.miny = self.miny.min(y);
        self.maxx = self.maxx.max(x);
        self.maxy = self.maxy.max(y);
    }

    #[inline]
    pub fn union(&mut self, o: &BBox) {
        if o.is_empty() {
            return;
        }
        self.minx = self.minx.min(o.minx);
        self.miny = self.miny.min(o.miny);
        self.maxx = self.maxx.max(o.maxx);
        self.maxy = self.maxy.max(o.maxy);
    }

    #[inline]
    pub fn unioned(mut self, o: &BBox) -> BBox {
        self.union(o);
        self
    }

    #[inline]
    pub fn intersects(&self, o: &BBox) -> bool {
        !self.is_empty()
            && !o.is_empty()
            && self.minx <= o.maxx
            && o.minx <= self.maxx
            && self.miny <= o.maxy
            && o.miny <= self.maxy
    }

    #[inline]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.minx && x <= self.maxx && y >= self.miny && y <= self.maxy
    }

    pub fn width(&self) -> i64 {
        if self.is_empty() { 0 } else { self.maxx as i64 - self.minx as i64 }
    }

    pub fn height(&self) -> i64 {
        if self.is_empty() { 0 } else { self.maxy as i64 - self.miny as i64 }
    }

    pub fn center(&self) -> (i32, i32) {
        (
            ((self.minx as i64 + self.maxx as i64) / 2) as i32,
            ((self.miny as i64 + self.maxy as i64) / 2) as i32,
        )
    }

    /// Approximate size in meters: `max(width_m, height_m)`.
    pub fn size_m(&self) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let (_, cy) = self.center();
        let coslat = (e7_to_deg(cy).to_radians()).cos().max(0.01);
        let w = self.width() as f64 * 1e-7 * 111_320.0 * coslat;
        let h = self.height() as f64 * 1e-7 * 110_540.0;
        w.max(h)
    }

    /// Approximate distance in meters between two boxes (0 if they intersect).
    pub fn distance_m(&self, o: &BBox) -> f64 {
        if self.is_empty() || o.is_empty() {
            return 0.0;
        }
        let dx = (o.minx as i64 - self.maxx as i64).max(self.minx as i64 - o.maxx as i64).max(0);
        let dy = (o.miny as i64 - self.maxy as i64).max(self.miny as i64 - o.maxy as i64).max(0);
        let lat = e7_to_deg(((self.center().1 as i64 + o.center().1 as i64) / 2) as i32);
        let coslat = lat.to_radians().cos().max(0.01);
        let x = dx as f64 * 1e-7 * 111_320.0 * coslat;
        let y = dy as f64 * 1e-7 * 110_540.0;
        (x * x + y * y).sqrt()
    }
}

#[inline]
pub fn deg_to_e7(d: f64) -> i32 {
    (d * 1e7).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[inline]
pub fn e7_to_deg(v: i32) -> f64 {
    v as f64 * 1e-7
}

/// Metadata of one object version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub version: u32,
    /// Unix seconds.
    pub timestamp: i64,
    pub changeset: u64,
    pub uid: u32,
    pub visible: bool,
    /// Copy of a version from a previous segment (for self-contained decoding).
    pub continuation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeVersion {
    pub meta: Meta,
    pub tags: Tags,
    /// Ignored if `!meta.visible`.
    pub lon: i32,
    pub lat: i32,
}

/// One version of a private node, stored inside its way's segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivEntry {
    pub timestamp: i64,
    pub lon: i32,
    pub lat: i32,
    pub visible: bool,
    pub continuation: bool,
    /// Zero if the file lacks `PRIVATE_META`.
    pub version: u32,
    pub changeset: u64,
    pub uid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub node_id: i64,
    /// `Some` for private nodes (their full time series within this segment).
    pub private: Option<Vec<PrivEntry>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WayVersion {
    pub meta: Meta,
    pub tags: Tags,
    /// Indexes into the segment's slot table.
    pub nodes: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Member {
    pub kind: ObjType,
    pub id: i64,
    pub role: Str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelVersion {
    pub meta: Meta,
    pub tags: Tags,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentBody {
    Node(Vec<NodeVersion>),
    Way { slots: Vec<Slot>, versions: Vec<WayVersion> },
    Relation(Vec<RelVersion>),
}

impl SegmentBody {
    pub fn obj_type(&self) -> ObjType {
        match self {
            SegmentBody::Node(_) => ObjType::Node,
            SegmentBody::Way { .. } => ObjType::Way,
            SegmentBody::Relation(_) => ObjType::Relation,
        }
    }

    pub fn n_versions(&self) -> usize {
        match self {
            SegmentBody::Node(v) => v.len(),
            SegmentBody::Way { versions, .. } => versions.len(),
            SegmentBody::Relation(v) => v.len(),
        }
    }
}

pub const SEG_MULTI: u8 = 1;
pub const SEG_LARGE: u8 = 2;
pub const SEG_NO_BBOX: u8 = 4;
/// Directory stores `tmin` explicitly (not derivable from the records).
pub const SEG_EXPLICIT_TMIN: u8 = 8;
/// Directory stores `tmax` explicitly.
pub const SEG_EXPLICIT_TMAX: u8 = 16;
/// The bbox is stored in the directory (always for ways/relations; for nodes only if it
/// is not the union of the node's positions).
pub const SEG_STORED_BBOX: u8 = 32;
/// Flags carried in the directory that describe the stored layout only.
pub const SEG_LAYOUT_FLAGS: u8 = SEG_NO_BBOX | SEG_EXPLICIT_TMIN | SEG_EXPLICIT_TMAX | SEG_STORED_BBOX;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub id: i64,
    pub flags: u8,
    /// Relative to `time_base`.
    pub tmin: u32,
    /// Relative to `time_base`; [`T_OPEN`] = open ended.
    pub tmax: u32,
    pub bbox: BBox,
    pub body: SegmentBody,
}

impl Segment {
    pub fn obj_type(&self) -> ObjType {
        self.body.obj_type()
    }
}

/// Converts an absolute timestamp into the clamped relative u32 used by indexes.
#[inline]
pub fn rel_time(t: i64, time_base: i64) -> u32 {
    (t - time_base).clamp(0, (T_OPEN - 1) as i64) as u32
}

/// Converts a relative u32 back to absolute seconds (`T_OPEN` → `i64::MAX`).
#[inline]
pub fn abs_time(t: u32, time_base: i64) -> i64 {
    if t == T_OPEN { i64::MAX } else { time_base + t as i64 }
}
