//! Conversion of query results to Arrow record batches.

use crate::query::{Feature, Geometry};
use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Float64Builder, Int32Builder, Int64Builder, ListBuilder, MapBuilder,
    StringBuilder, TimestampSecondBuilder,
};
use arrow_array::{ArrayRef, Int64Array, ListArray, RecordBatch, StringArray, StructArray};
use arrow_buffer::{NullBuffer, OffsetBuffer};
use arrow_schema::{ArrowError, DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use knut_format::e7_to_deg;
use std::sync::Arc;

fn member_fields() -> Fields {
    Fields::from(vec![
        Field::new("type", DataType::Utf8, false),
        Field::new("ref", DataType::Int64, false),
        Field::new("role", DataType::Utf8, false),
    ])
}

/// Schema of feature batches.
pub fn feature_schema() -> SchemaRef {
    let map_entries = Field::new(
        "entries",
        DataType::Struct(Fields::from(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("value", DataType::Utf8, true),
        ])),
        false,
    );
    Arc::new(Schema::new(vec![
        Field::new("type", DataType::Utf8, false),
        Field::new("id", DataType::Int64, false),
        Field::new("version", DataType::Int32, false),
        Field::new("timestamp", DataType::Timestamp(TimeUnit::Second, Some("UTC".into())), false),
        Field::new("changeset", DataType::Int64, false),
        Field::new("uid", DataType::Int64, false),
        Field::new("visible", DataType::Boolean, false),
        Field::new("tags", DataType::Map(Arc::new(map_entries), false), false),
        Field::new("lon", DataType::Float64, true),
        Field::new("lat", DataType::Float64, true),
        Field::new("nodes", DataType::List(Arc::new(Field::new("item", DataType::Int64, true))), true),
        Field::new(
            "members",
            DataType::List(Arc::new(Field::new("item", DataType::Struct(member_fields()), true))),
            true,
        ),
        Field::new("geometry", DataType::Binary, true),
    ]))
}

/// WKB (little endian) for a feature's geometry; `None` if it has none.
pub fn wkb(g: &Geometry) -> Option<Vec<u8>> {
    match g {
        Geometry::None => None,
        Geometry::Point(x, y) => {
            let mut b = Vec::with_capacity(21);
            b.push(1);
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&e7_to_deg(*x).to_le_bytes());
            b.extend_from_slice(&e7_to_deg(*y).to_le_bytes());
            Some(b)
        }
        Geometry::Line(pts) => {
            let pts: Vec<&(i32, i32)> = pts.iter().flatten().collect();
            if pts.len() < 2 {
                return None;
            }
            let mut b = Vec::with_capacity(9 + 16 * pts.len());
            b.push(1);
            b.extend_from_slice(&2u32.to_le_bytes());
            b.extend_from_slice(&(pts.len() as u32).to_le_bytes());
            for (x, y) in pts {
                b.extend_from_slice(&e7_to_deg(*x).to_le_bytes());
                b.extend_from_slice(&e7_to_deg(*y).to_le_bytes());
            }
            Some(b)
        }
    }
}

/// Converts features into one record batch.
pub fn features_to_batch(features: &[Feature]) -> Result<RecordBatch, ArrowError> {
    let n = features.len();
    let mut typ = StringBuilder::with_capacity(n, n * 4);
    let mut id = Int64Builder::with_capacity(n);
    let mut version = Int32Builder::with_capacity(n);
    let mut ts = TimestampSecondBuilder::with_capacity(n).with_timezone("UTC");
    let mut cs = Int64Builder::with_capacity(n);
    let mut uid = Int64Builder::with_capacity(n);
    let mut vis = BooleanBuilder::with_capacity(n);
    let mut tags = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    let mut lon = Float64Builder::with_capacity(n);
    let mut lat = Float64Builder::with_capacity(n);
    let mut nodes = ListBuilder::new(Int64Builder::new());
    let mut geom = BinaryBuilder::new();
    let mut m_off: Vec<i32> = vec![0];
    let mut m_valid: Vec<bool> = Vec::with_capacity(n);
    let (mut m_type, mut m_ref, mut m_role) = (Vec::new(), Vec::new(), Vec::new());

    for f in features {
        typ.append_value(f.obj_type.name());
        id.append_value(f.id);
        version.append_value(f.meta.version as i32);
        ts.append_value(f.meta.timestamp);
        cs.append_value(f.meta.changeset as i64);
        uid.append_value(f.meta.uid as i64);
        vis.append_value(f.meta.visible);
        for (k, v) in f.tags.iter() {
            tags.keys().append_value(k);
            tags.values().append_value(v);
        }
        tags.append(true)?;
        match f.geometry {
            Geometry::Point(x, y) => {
                lon.append_value(e7_to_deg(x));
                lat.append_value(e7_to_deg(y));
            }
            _ => {
                lon.append_null();
                lat.append_null();
            }
        }
        if f.obj_type == knut_format::ObjType::Way {
            nodes.values().append_slice(&f.nodes);
            nodes.append(true);
        } else {
            nodes.append(false);
        }
        if f.obj_type == knut_format::ObjType::Relation {
            for m in &f.members {
                m_type.push(m.kind.name());
                m_ref.push(m.id);
                m_role.push(m.role.to_string());
            }
            m_valid.push(true);
        } else {
            m_valid.push(false);
        }
        m_off.push(m_type.len() as i32);
        match wkb(&f.geometry) {
            Some(b) => geom.append_value(b),
            None => geom.append_null(),
        }
    }
    let members_struct = StructArray::new(
        member_fields(),
        vec![
            Arc::new(StringArray::from(m_type)) as ArrayRef,
            Arc::new(Int64Array::from(m_ref)) as ArrayRef,
            Arc::new(StringArray::from(m_role)) as ArrayRef,
        ],
        None,
    );
    let members = ListArray::try_new(
        Arc::new(Field::new("item", DataType::Struct(member_fields()), true)),
        OffsetBuffer::new(m_off.into()),
        Arc::new(members_struct),
        Some(NullBuffer::from(m_valid)),
    )?;
    RecordBatch::try_new(
        feature_schema(),
        vec![
            Arc::new(typ.finish()),
            Arc::new(id.finish()),
            Arc::new(version.finish()),
            Arc::new(ts.finish()),
            Arc::new(cs.finish()),
            Arc::new(uid.finish()),
            Arc::new(vis.finish()),
            Arc::new(tags.finish()),
            Arc::new(lon.finish()),
            Arc::new(lat.finish()),
            Arc::new(nodes.finish()),
            Arc::new(members),
            Arc::new(geom.finish()),
        ],
    )
}

/// Converts features into batches of at most `batch_size` rows.
pub fn features_to_batches(features: &[Feature], batch_size: usize) -> Result<Vec<RecordBatch>, ArrowError> {
    if features.is_empty() {
        return Ok(vec![features_to_batch(&[])?]);
    }
    features.chunks(batch_size.max(1)).map(features_to_batch).collect()
}
