//! Baseline: export a history PBF to Parquet (one row per version, with validity
//! interval) for comparison with DuckDB.

use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int32Builder, Int64Builder, ListBuilder, MapBuilder, StringBuilder,
    TimestampSecondBuilder,
};
use arrow_array::{ArrayRef, Int64Array, ListArray, RecordBatch, StringArray, StructArray};
use arrow_buffer::OffsetBuffer;
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use knut_builder::pbf::PbfFile;
use knut_builder::{Element, Source};
use knut_format::{ObjType, e7_to_deg};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

fn ts_type() -> DataType {
    DataType::Timestamp(TimeUnit::Second, Some("UTC".into()))
}

fn member_fields() -> Fields {
    Fields::from(vec![
        Field::new("type", DataType::Utf8, false),
        Field::new("ref", DataType::Int64, false),
        Field::new("role", DataType::Utf8, false),
    ])
}

fn schema(t: ObjType) -> SchemaRef {
    let entries = Field::new(
        "entries",
        DataType::Struct(Fields::from(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("value", DataType::Utf8, true),
        ])),
        false,
    );
    let mut f = vec![
        Field::new("id", DataType::Int64, false),
        Field::new("version", DataType::Int32, false),
        Field::new("valid_from", ts_type(), false),
        Field::new("valid_to", ts_type(), true),
        Field::new("changeset", DataType::Int64, false),
        Field::new("uid", DataType::Int64, false),
        Field::new("visible", DataType::Boolean, false),
        Field::new("tags", DataType::Map(Arc::new(entries), false), false),
    ];
    match t {
        ObjType::Node => {
            f.push(Field::new("lon", DataType::Float64, true));
            f.push(Field::new("lat", DataType::Float64, true));
        }
        ObjType::Way => f.push(Field::new("nodes", DataType::List(Arc::new(Field::new("item", DataType::Int64, true))), false)),
        ObjType::Relation => f.push(Field::new(
            "members",
            DataType::List(Arc::new(Field::new("item", DataType::Struct(member_fields()), true))),
            false,
        )),
    }
    Arc::new(Schema::new(f))
}

fn batch(t: ObjType, rows: &[(Element, Option<i64>)]) -> Result<RecordBatch, String> {
    let mut id = Int64Builder::new();
    let mut version = Int32Builder::new();
    let mut vf = TimestampSecondBuilder::new().with_timezone("UTC");
    let mut vt = TimestampSecondBuilder::new().with_timezone("UTC");
    let mut cs = Int64Builder::new();
    let mut uid = Int64Builder::new();
    let mut vis = BooleanBuilder::new();
    let mut tags = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    let mut lon = Float64Builder::new();
    let mut lat = Float64Builder::new();
    let mut nodes = ListBuilder::new(Int64Builder::new());
    let (mut m_off, mut m_type, mut m_ref, mut m_role) = (vec![0i32], Vec::new(), Vec::new(), Vec::<String>::new());
    for (e, valid_to) in rows {
        let m = e.meta();
        id.append_value(e.id());
        version.append_value(m.version as i32);
        vf.append_value(m.timestamp);
        vt.append_option(*valid_to);
        cs.append_value(m.changeset as i64);
        uid.append_value(m.uid as i32 as i64);
        vis.append_value(m.visible);
        let tg = match e {
            Element::Node(n) => &n.tags,
            Element::Way(w) => &w.tags,
            Element::Relation(r) => &r.tags,
        };
        for (k, v) in tg.iter() {
            tags.keys().append_value(k);
            tags.values().append_value(v);
        }
        tags.append(true).map_err(|e| e.to_string())?;
        match e {
            Element::Node(n) => {
                if m.visible {
                    lon.append_value(e7_to_deg(n.lon));
                    lat.append_value(e7_to_deg(n.lat));
                } else {
                    lon.append_null();
                    lat.append_null();
                }
            }
            Element::Way(w) => {
                nodes.values().append_slice(&w.refs);
                nodes.append(true);
            }
            Element::Relation(r) => {
                for mm in &r.members {
                    m_type.push(mm.kind.name());
                    m_ref.push(mm.id);
                    m_role.push(mm.role.to_string());
                }
                m_off.push(m_type.len() as i32);
            }
        }
    }
    let mut cols: Vec<ArrayRef> = vec![
        Arc::new(id.finish()),
        Arc::new(version.finish()),
        Arc::new(vf.finish()),
        Arc::new(vt.finish()),
        Arc::new(cs.finish()),
        Arc::new(uid.finish()),
        Arc::new(vis.finish()),
        Arc::new(tags.finish()),
    ];
    match t {
        ObjType::Node => {
            cols.push(Arc::new(lon.finish()));
            cols.push(Arc::new(lat.finish()));
        }
        ObjType::Way => cols.push(Arc::new(nodes.finish())),
        ObjType::Relation => {
            let st = StructArray::new(
                member_fields(),
                vec![
                    Arc::new(StringArray::from(m_type)) as ArrayRef,
                    Arc::new(Int64Array::from(m_ref)) as ArrayRef,
                    Arc::new(StringArray::from(m_role)) as ArrayRef,
                ],
                None,
            );
            let l = ListArray::try_new(
                Arc::new(Field::new("item", DataType::Struct(member_fields()), true)),
                OffsetBuffer::new(m_off.into()),
                Arc::new(st),
                None,
            )
            .map_err(|e| e.to_string())?;
            cols.push(Arc::new(l));
        }
    }
    RecordBatch::try_new(schema(t), cols).map_err(|e| e.to_string())
}

pub fn export(pbf: &Path, dir: &Path, zstd_level: i32) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let src = PbfFile::open(pbf).map_err(|e| e.to_string())?;
    for t in ObjType::ALL {
        let path = dir.join(format!("{}s.parquet", t.name()));
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(zstd_level).map_err(|e| e.to_string())?))
            .set_max_row_group_row_count(Some(1 << 20))
            .build();
        let f = File::create(&path).map_err(|e| e.to_string())?;
        let mut w = ArrowWriter::try_new(f, schema(t), Some(props)).map_err(|e| e.to_string())?;
        let mut pending: Option<Element> = None;
        let mut rows: Vec<(Element, Option<i64>)> = Vec::new();
        let mut err: Option<String> = None;
        let mut take = |batch_in: Vec<Element>| -> knut_builder::Result<()> {
            for e in batch_in {
                if let Some(p) = pending.take() {
                    let vt = (p.id() == e.id()).then(|| e.meta().timestamp);
                    rows.push((p, vt));
                }
                pending = Some(e);
            }
            if rows.len() >= 1 << 18 {
                match batch(t, &rows).and_then(|b| w.write(&b).map_err(|e| e.to_string())) {
                    Ok(()) => rows.clear(),
                    Err(e) => err = Some(e),
                }
            }
            Ok(())
        };
        match t {
            ObjType::Node => src.nodes(&mut take),
            ObjType::Way => src.ways(&mut take),
            ObjType::Relation => src.relations(&mut take),
        }
        .map_err(|e| e.to_string())?;
        if let Some(e) = err {
            return Err(e);
        }
        if let Some(p) = pending.take() {
            rows.push((p, None));
        }
        if !rows.is_empty() {
            w.write(&batch(t, &rows)?).map_err(|e| e.to_string())?;
        }
        w.close().map_err(|e| e.to_string())?;
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}
