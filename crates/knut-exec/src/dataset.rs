//! External data sets (`dataset 'file'`, KNUT_LANG.md §5.6): CSV, GeoJSON and Parquet.
//! Columns become tags, a geometry column (WKT/WKB) or lon/lat columns the geometry.

use crate::eval::{Ctx, lock};
use crate::geom::{self, Geom, P, Polygon};
use crate::set::SetV;
use crate::value::*;
use arrow_array::{Array, BinaryArray, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, StringArray};
use knut_lang::diag::{Diagnostic, Span, Text2};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn e213(span: Span, file: &str, why: String) -> RtErr {
    RtErr::fatal(
        Diagnostic::error(
            "E213",
            span,
            Text2::new(format!("The data set '{file}' cannot be read."), format!("Der Datensatz '{file}' lässt sich nicht lesen.")),
        )
        .why(why.clone(), why),
    )
}

pub fn resolve(ctx: &Ctx, file: &str) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() { p.to_path_buf() } else { ctx.base_dir.join(p) }
}

pub fn load(ctx: &Ctx, file: &str, span: Span) -> R<Value> {
    if let Some(v) = lock(&ctx.caches.datasets).get(file) {
        return v.clone();
    }
    let path = resolve(ctx, file);
    let label: Arc<str> = Arc::from(file);
    let rows = match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("csv") | Some("tsv") | Some("txt") => read_csv(&path),
        Some("geojson") | Some("json") => read_geojson(&path),
        Some("parquet") | Some("pq") => read_parquet(&path),
        _ => Err("Supported are .csv, .geojson and .parquet files.".to_string()),
    };
    let r = rows.map_err(|e| e213(span, file, e)).map(|rows| {
        let vals: Vec<Value> = rows
            .into_iter()
            .enumerate()
            .map(|(i, (tags, g))| {
                let mut v = Ver::empty();
                v.tags = tags.into_iter().map(|(k, x)| (Arc::from(k.as_str()), Arc::from(x.as_str()))).collect::<Vec<_>>().into();
                let mut e = Elem::new(EKind::Row, None, i as i64 + 1, v, 0);
                e.extra = Extra::Row(label.clone());
                let _ = e.geom.set(Arc::new(g.unwrap_or(Geom::Empty)));
                Value::Elem(Arc::new(e))
            })
            .collect();
        Value::Set(Arc::new(SetV::from_vec(vals)))
    });
    lock(&ctx.caches.datasets).insert(file.to_string(), r.clone());
    r
}

type Row = (Vec<(String, String)>, Option<Geom>);

fn geometry_from(cols: &[(String, String)]) -> Option<Geom> {
    let get = |names: &[&str]| -> Option<&str> {
        cols.iter().find(|(k, _)| names.contains(&k.to_ascii_lowercase().as_str())).map(|(_, v)| v.as_str())
    };
    if let Some(w) = get(&["geometry", "wkt", "geom", "the_geom"]) {
        if let Some(g) = geom::parse_wkt(w) {
            return Some(g);
        }
    }
    let lon = get(&["lon", "lng", "longitude", "x"])?.trim().parse::<f64>().ok()?;
    let lat = get(&["lat", "latitude", "y"])?.trim().parse::<f64>().ok()?;
    Some(Geom::Point((lon, lat)))
}

fn read_csv(path: &Path) -> Result<Vec<Row>, String> {
    let delim = if path.extension().map_or(false, |e| e == "tsv") { b'\t' } else { b',' };
    let mut rd = csv::ReaderBuilder::new().delimiter(delim).flexible(true).from_path(path).map_err(|e| e.to_string())?;
    let headers: Vec<String> = rd.headers().map_err(|e| e.to_string())?.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    for rec in rd.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        let cols: Vec<(String, String)> = headers.iter().zip(rec.iter()).filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.clone(), v.to_string())).collect();
        let g = geometry_from(&cols);
        out.push((cols, g));
    }
    Ok(out)
}

fn json_geom(g: &serde_json::Value) -> Option<Geom> {
    let t = g.get("type")?.as_str()?;
    let c = g.get("coordinates");
    let pt = |v: &serde_json::Value| -> Option<P> { Some((v.get(0)?.as_f64()?, v.get(1)?.as_f64()?)) };
    let line = |v: &serde_json::Value| -> Option<Vec<P>> { v.as_array()?.iter().map(pt).collect() };
    let poly = |v: &serde_json::Value| -> Option<Polygon> {
        let mut rings: Vec<Vec<P>> = v.as_array()?.iter().map(line).collect::<Option<_>>()?;
        if rings.is_empty() {
            return None;
        }
        let outer = rings.remove(0);
        Some(Polygon { outer, holes: rings })
    };
    Some(match t {
        "Point" => Geom::Point(pt(c?)?),
        "LineString" => Geom::Line(line(c?)?),
        "MultiLineString" => Geom::Multi(c?.as_array()?.iter().filter_map(|l| line(l).map(Geom::Line)).collect()),
        "Polygon" => Geom::Poly(vec![poly(c?)?]),
        "MultiPolygon" => Geom::Poly(c?.as_array()?.iter().filter_map(poly).collect()),
        "MultiPoint" => Geom::Multi(c?.as_array()?.iter().filter_map(|p| pt(p).map(Geom::Point)).collect()),
        "GeometryCollection" => Geom::Multi(g.get("geometries")?.as_array()?.iter().filter_map(json_geom).collect()),
        _ => return None,
    })
}

fn read_geojson(path: &Path) -> Result<Vec<Row>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let feats = match v.get("features").and_then(|f| f.as_array()) {
        Some(f) => f.clone(),
        None => vec![v],
    };
    let mut out = Vec::new();
    for f in feats {
        let mut cols = Vec::new();
        if let Some(p) = f.get("properties").and_then(|p| p.as_object()) {
            for (k, x) in p {
                let s = match x {
                    serde_json::Value::Null => continue,
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                cols.push((k.clone(), s));
            }
        }
        if let Some(id) = f.get("id") {
            cols.push(("id".into(), id.as_str().map(|s| s.to_string()).unwrap_or_else(|| id.to_string())));
        }
        let g = f.get("geometry").and_then(json_geom);
        out.push((cols, g));
    }
    Ok(out)
}

/// Minimal WKB reader (points, lines, polygons and their multi forms).
pub fn parse_wkb(b: &[u8]) -> Option<Geom> {
    fn rd(b: &[u8], p: &mut usize) -> Option<Geom> {
        let le = *b.get(*p)? == 1;
        *p += 1;
        let u32_at = |b: &[u8], p: &mut usize| -> Option<u32> {
            let s: [u8; 4] = b.get(*p..*p + 4)?.try_into().ok()?;
            *p += 4;
            Some(if le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
        };
        let f64_at = |b: &[u8], p: &mut usize| -> Option<f64> {
            let s: [u8; 8] = b.get(*p..*p + 8)?.try_into().ok()?;
            *p += 8;
            Some(if le { f64::from_le_bytes(s) } else { f64::from_be_bytes(s) })
        };
        let t = u32_at(b, p)? % 1000;
        let pts = |b: &[u8], p: &mut usize| -> Option<Vec<P>> {
            let n = u32_at(b, p)? as usize;
            (0..n).map(|_| Some((f64_at(b, p)?, f64_at(b, p)?))).collect()
        };
        Some(match t {
            1 => Geom::Point((f64_at(b, p)?, f64_at(b, p)?)),
            2 => Geom::Line(pts(b, p)?),
            3 => {
                let n = u32_at(b, p)? as usize;
                let mut rings: Vec<Vec<P>> = (0..n).map(|_| pts(b, p)).collect::<Option<_>>()?;
                if rings.is_empty() {
                    return Some(Geom::Empty);
                }
                let outer = rings.remove(0);
                Geom::Poly(vec![Polygon { outer, holes: rings }])
            }
            4..=7 => {
                let n = u32_at(b, p)? as usize;
                let parts: Vec<Geom> = (0..n).map(|_| rd(b, p)).collect::<Option<_>>()?;
                if t == 6 {
                    Geom::Poly(parts.into_iter().flat_map(|g| if let Geom::Poly(ps) = g { ps } else { vec![] }).collect())
                } else {
                    Geom::Multi(parts)
                }
            }
            _ => return None,
        })
    }
    let mut p = 0;
    rd(b, &mut p)
}

fn read_parquet(path: &Path) -> Result<Vec<Row>, String> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let rd = ParquetRecordBatchReaderBuilder::try_new(f).map_err(|e| e.to_string())?.build().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for batch in rd {
        let batch = batch.map_err(|e| e.to_string())?;
        let schema = batch.schema();
        let n = batch.num_rows();
        let mut rows: Vec<Row> = (0..n).map(|_| (Vec::new(), None)).collect();
        for (ci, field) in schema.fields().iter().enumerate() {
            let col = batch.column(ci);
            let name = field.name().clone();
            let is_geom = matches!(name.to_ascii_lowercase().as_str(), "geometry" | "geom" | "wkb");
            for (r, row) in rows.iter_mut().enumerate() {
                if col.is_null(r) {
                    continue;
                }
                let any = col.as_any();
                if is_geom {
                    if let Some(a) = any.downcast_ref::<BinaryArray>() {
                        row.1 = parse_wkb(a.value(r));
                        continue;
                    }
                    if let Some(a) = any.downcast_ref::<LargeBinaryArray>() {
                        row.1 = parse_wkb(a.value(r));
                        continue;
                    }
                }
                let s = if let Some(a) = any.downcast_ref::<StringArray>() {
                    a.value(r).to_string()
                } else if let Some(a) = any.downcast_ref::<LargeStringArray>() {
                    a.value(r).to_string()
                } else if let Some(a) = any.downcast_ref::<Int64Array>() {
                    a.value(r).to_string()
                } else if let Some(a) = any.downcast_ref::<Int32Array>() {
                    a.value(r).to_string()
                } else if let Some(a) = any.downcast_ref::<Float64Array>() {
                    a.value(r).to_string()
                } else if let Some(a) = any.downcast_ref::<Float32Array>() {
                    a.value(r).to_string()
                } else if let Some(a) = any.downcast_ref::<BooleanArray>() {
                    a.value(r).to_string()
                } else {
                    continue;
                };
                row.0.push((name.clone(), s));
            }
        }
        for mut row in rows {
            if row.1.is_none() {
                row.1 = geometry_from(&row.0);
            }
            out.push(row);
        }
    }
    Ok(out)
}
