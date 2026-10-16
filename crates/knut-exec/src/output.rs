//! Result output: text tables, CSV, JSON and Parquet (KNUT_LANG.md §1).

use crate::eval::Ctx;
use crate::geom;
use crate::set::Hints;
use crate::value::*;
use knut_lang::dates;
use std::fmt::Write as _;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Csv,
    Json,
    Parquet,
}

/// A table: column names and rows of values.
pub struct Table {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

fn elem_cols() -> Vec<String> {
    ["type", "id", "version", "time", "name", "tags"].iter().map(|s| s.to_string()).collect()
}

fn tags_text(e: &Elem) -> String {
    e.v.tags.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";")
}

fn elem_row(e: &Elem) -> Vec<Value> {
    let t = match &e.extra {
        Extra::Changeset(c) => c.t0,
        Extra::Note(n) => n.created,
        _ => e.v.ts,
    };
    let name = match &e.extra {
        Extra::User(u) => u.name.clone().map_or(Value::None, Value::Text),
        Extra::Key(k) => Value::Text(k.name.clone()),
        _ => e.tag("name").map_or(Value::None, |s| Value::Text(s.clone())),
    };
    vec![
        Value::text(e.kind.name()),
        Value::num(e.id as f64),
        if e.v.version > 0 { Value::num(e.v.version as f64) } else { Value::None },
        if t != 0 { Value::instant(t) } else { Value::None },
        name,
        Value::text(&tags_text(e)),
    ]
}

/// Turns a value into a table.
pub fn table(ctx: &Ctx, v: &Value) -> R<Table> {
    Ok(match v {
        Value::Grouped(g) => {
            let mut cols = if g.key_names.is_empty() { vec!["key".to_string()] } else { g.key_names.clone() };
            let nk = cols.len();
            cols.push(g.value_name.clone());
            let rows = g
                .rows
                .iter()
                .map(|r| {
                    let mut row = match &r.key {
                        Value::List(ks) if nk > 1 => ks.iter().cloned().collect(),
                        k => vec![k.clone()],
                    };
                    while row.len() < nk {
                        row.push(Value::None);
                    }
                    row.push(r.value.clone());
                    row
                })
                .collect();
            Table { cols, rows }
        }
        Value::Set(_) | Value::List(_) => {
            let items = ctx.items_of(v, Hints::default(), knut_lang::diag::Span::default())?;
            if items.iter().all(|x| matches!(x, Value::Elem(_))) && !items.is_empty() {
                Table { cols: elem_cols(), rows: items.iter().map(|x| if let Value::Elem(e) = x { elem_row(e) } else { vec![] }).collect() }
            } else if items.iter().all(|x| matches!(x, Value::Tuple(_))) && !items.is_empty() {
                let Value::Tuple(t0) = &items[0] else { unreachable!() };
                let mut cols = vec!["type".to_string(), "id".to_string()];
                for n in t0.names.iter() {
                    cols.push(format!("{n}.version"));
                    cols.push(format!("{n}.time"));
                }
                let rows = items
                    .iter()
                    .map(|x| {
                        let Value::Tuple(t) = x else { unreachable!() };
                        let e0 = &t.items[0];
                        let mut row = vec![Value::text(e0.otype.map_or(e0.kind.name(), |o| o.name())), Value::num(e0.id as f64)];
                        for e in &t.items {
                            row.push(Value::num(e.v.version as f64));
                            row.push(Value::instant(e.v.ts));
                        }
                        row
                    })
                    .collect();
                Table { cols, rows }
            } else if items.iter().all(|x| matches!(x, Value::Group(_))) && !items.is_empty() {
                // `groups g in … with …`
                let nk = items.iter().map(|x| if let Value::Group(g) = x && let Value::List(k) = &g.key { k.len() } else { 1 }).max().unwrap_or(1);
                let mut cols: Vec<String> = if nk == 1 { vec!["key".into()] } else { (1..=nk).map(|i| format!("key{i}")).collect() };
                cols.push("value".into());
                let rows = items
                    .iter()
                    .map(|x| {
                        let Value::Group(g) = x else { unreachable!() };
                        let mut row: Vec<Value> = match &g.key {
                            Value::List(k) if nk > 1 => k.iter().cloned().collect(),
                            k => vec![k.clone()],
                        };
                        row.resize(nk, Value::None);
                        row.push(g.value.clone());
                        row
                    })
                    .collect();
                Table { cols, rows }
            } else if let Some(n) = list_width(&items) {
                // `xs.map(x => [a, b, c])`: one column per list position
                let rows = items.iter().map(|x| if let Value::List(l) = x { l.iter().cloned().collect() } else { vec![] }).collect();
                Table { cols: (1..=n).map(|i| i.to_string()).collect(), rows }
            } else {
                Table { cols: vec!["value".into()], rows: items.iter().map(|x| vec![x.clone()]).collect() }
            }
        }
        Value::Record(r) => Table { cols: r.iter().map(|(k, _)| k.clone()).collect(), rows: vec![r.iter().map(|(_, v)| v.clone()).collect()] },
        Value::Elem(e) => Table { cols: elem_cols(), rows: vec![elem_row(e)] },
        other => Table { cols: vec!["value".into()], rows: vec![vec![other.clone()]] },
    })
}

/// Common length of items that are all short lists (2 to 32 entries).
fn list_width(items: &[Value]) -> Option<usize> {
    let mut n = None;
    for x in items {
        let Value::List(l) = x else { return None };
        if n.is_some_and(|n| n != l.len()) || !(2..=32).contains(&l.len()) {
            return None;
        }
        n = Some(l.len());
    }
    n
}

fn cell(v: &Value) -> String {
    match v {
        Value::Elem(e) if e.hit_text().is_some() => {
            let l = e.label();
            let hit = e.hit_text().unwrap_or_default();
            match e.tag("name") {
                Some(n) => format!("{l} ({n}) · {hit}"),
                None => format!("{l} · {hit}"),
            }
        }
        Value::Elem(e) => {
            let l = e.label();
            match e.tag("name") {
                Some(n) => format!("{l} ({n})"),
                None => match &e.extra {
                    Extra::User(u) => u.name.as_ref().map_or(l.clone(), |n| format!("{l} ({n})")),
                    Extra::Key(k) => format!("key {}", k.name),
                    _ => l,
                },
            }
        }
        Value::Num(n) if n.dim.is_none() && n.unit.is_none() && n.v.fract() == 0.0 && n.v.abs() >= 10_000.0 => knut_plan::fmt_int(n.v),
        other => other.display(),
    }
}

/// Text rendering for the terminal.
pub fn text(ctx: &Ctx, v: &Value, limit: usize) -> R<String> {
    let mut s = String::new();
    match v {
        Value::Set(_) | Value::List(_) | Value::Grouped(_) => {
            let t = table(ctx, v)?;
            let n = t.rows.len();
            let what = match v {
                Value::Grouped(_) => (if n == 1 { "group" } else { "groups" }, if n == 1 { "Gruppe" } else { "Gruppen" }),
                Value::List(_) => (if n == 1 { "item" } else { "items" }, if n == 1 { "Eintrag" } else { "Einträge" }),
                _ => (if n == 1 { "element" } else { "elements" }, if n == 1 { "Element" } else { "Elemente" }),
            };
            if n == 0 {
                let _ = writeln!(s, "{}", if ctx.de() { "(leer)" } else { "(empty)" });
                return Ok(s);
            }
            let simple = t.cols.len() == 6 && t.cols[0] == "type" && !matches!(v, Value::Grouped(_));
            if simple {
                let _ = writeln!(s, "{} {}", knut_plan::fmt_int(n as f64), if ctx.de() { what.1 } else { what.0 });
                let items = ctx.items_of(v, Hints::default(), knut_lang::diag::Span::default())?;
                for it in items.iter().take(limit) {
                    let _ = writeln!(s, "  {}", cell(it));
                }
                if n > limit {
                    let _ = writeln!(s, "  … {} {}", knut_plan::fmt_int((n - limit) as f64), if ctx.de() { "weitere" } else { "more" });
                }
                return Ok(s);
            }
            // identifiers and list positions are printed without digit grouping
            let plain: Vec<bool> = t.cols.iter().map(|c| c == "id" || c.ends_with(".id") || c.ends_with("version") || c.chars().all(|x| x.is_ascii_digit())).collect();
            // digit grouping for a whole column once one of its integers needs it
            let big = |v: &Value| matches!(v, Value::Num(n) if n.dim.is_none() && n.unit.is_none() && n.v.fract() == 0.0 && n.v.abs() >= 10_000.0);
            let group: Vec<bool> = (0..t.cols.len()).map(|i| t.rows.iter().take(limit).any(|r| r.get(i).is_some_and(big))).collect();
            let rows: Vec<Vec<String>> = t
                .rows
                .iter()
                .take(limit)
                .map(|r| {
                    r.iter()
                        .enumerate()
                        .map(|(i, v)| match v {
                            Value::Num(_) if plain.get(i).copied().unwrap_or(false) => v.display(),
                            Value::Num(n) if group[i] && n.dim.is_none() && n.unit.is_none() && n.v.fract() == 0.0 => knut_plan::fmt_int(n.v),
                            _ => cell(v),
                        })
                        .collect()
                })
                .collect();
            let mut w: Vec<usize> = t.cols.iter().map(|c| c.chars().count()).collect();
            for r in &rows {
                for (i, c) in r.iter().enumerate() {
                    if i < w.len() {
                        w[i] = w[i].max(c.chars().count()).min(60);
                    }
                }
            }
            let numeric: Vec<bool> = (0..t.cols.len()).map(|i| t.rows.iter().all(|r| matches!(r.get(i), Some(Value::Num(_)) | Some(Value::None)))).collect();
            let line = |cells: &[String], s: &mut String| {
                let mut l = String::new();
                for (i, c) in cells.iter().enumerate() {
                    let c: String = if c.chars().count() > 60 { c.chars().take(59).chain(['…']).collect() } else { c.clone() };
                    let pad = w[i].saturating_sub(c.chars().count());
                    if numeric[i] {
                        l.push_str(&" ".repeat(pad));
                        l.push_str(&c);
                    } else {
                        l.push_str(&c);
                        l.push_str(&" ".repeat(pad));
                    }
                    l.push_str("  ");
                }
                let _ = writeln!(s, "{}", l.trim_end());
            };
            line(&t.cols, &mut s);
            let total: usize = w.iter().sum::<usize>() + 2 * w.len().saturating_sub(1);
            let _ = writeln!(s, "{}", "─".repeat(total.min(120)));
            for r in &rows {
                line(r, &mut s);
            }
            if n > limit {
                let _ = writeln!(s, "… {} {} {}", knut_plan::fmt_int((n - limit) as f64), if ctx.de() { "weitere" } else { "more" }, if ctx.de() { what.1 } else { what.0 });
            }
        }
        Value::Record(r) => {
            for (k, x) in r.iter() {
                match x {
                    Value::Grouped(_) | Value::Set(_) | Value::List(_) => {
                        let _ = writeln!(s, "{k}:");
                        for l in text(ctx, x, limit)?.lines() {
                            let _ = writeln!(s, "  {l}");
                        }
                    }
                    _ => {
                        let _ = writeln!(s, "{k}: {}", cell(x));
                    }
                }
            }
        }
        Value::Elem(e) => {
            let _ = writeln!(s, "{}", cell(v));
            for (k, x) in e.v.tags.iter() {
                let _ = writeln!(s, "  {k}={x}");
            }
        }
        other => {
            let _ = writeln!(s, "{}", cell(other));
        }
    }
    Ok(s)
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', ';']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

fn plain(v: &Value) -> String {
    match v {
        Value::None => String::new(),
        Value::Num(n) => fmt_plain(n.shown()),
        Value::Time(d) => dates::format(d),
        Value::Elem(e) => e.label(),
        other => other.display(),
    }
}

fn fmt_plain(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 { format!("{}", x as i64) } else { format!("{x}") }
}

pub fn csv(ctx: &Ctx, v: &Value) -> R<String> {
    let t = table(ctx, v)?;
    let mut s = String::new();
    let mut cols = t.cols.clone();
    // units go into the header
    for (i, c) in cols.iter_mut().enumerate() {
        if let Some(Value::Num(n)) = t.rows.iter().find_map(|r| r.get(i).filter(|x| !x.is_none())) {
            let u = n.unit_name();
            if !u.is_empty() {
                *c = format!("{c} [{u}]");
            }
        }
    }
    let _ = writeln!(s, "{}", cols.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(","));
    for r in &t.rows {
        let _ = writeln!(s, "{}", r.iter().map(|x| csv_escape(&plain(x))).collect::<Vec<_>>().join(","));
    }
    Ok(s)
}

pub fn json(ctx: &Ctx, v: &Value) -> R<serde_json::Value> {
    use serde_json::{Value as J, json};
    Ok(match v {
        Value::None => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Num(n) => {
            let u = n.unit_name();
            let x = n.shown();
            // integers stay integers in JSON
            let x = if x.fract() == 0.0 && x.abs() < 9.0e15 { json!(x as i64) } else { json!(x) };
            if u.is_empty() { x } else { json!({"value": x, "unit": u}) }
        }
        Value::Text(s) => J::String(s.to_string()),
        Value::Tag(t) => J::String(t.text.to_string()),
        Value::Time(d) => J::String(dates::format(d)),
        Value::Elem(e) => {
            match &e.extra {
                Extra::Key(k) => return Ok(json!({"type": "key", "name": k.name.as_ref(), "count": k.count, "values": k.values})),
                Extra::User(u) => return Ok(json!({"type": "user", "id": u.uid, "name": u.name.as_deref(), "changesets": u.changesets.len(), "edits": u.edits})),
                _ => {}
            }
            let tags: serde_json::Map<String, J> = e.v.tags.iter().map(|(k, x)| (k.to_string(), J::String(x.to_string()))).collect();
            let mut o = json!({"type": e.kind.name(), "id": e.id});
            if e.v.version > 0 {
                o["version"] = json!(e.v.version);
                o["time"] = json!(dates::format_instant(e.v.ts));
            }
            if !tags.is_empty() {
                o["tags"] = J::Object(tags);
            }
            // OSM elements decoded without geometry load it now
            let g = if e.otype.is_some() { Some(ctx.elem_geom(e)) } else { e.geom.get().cloned() };
            if let Some(g) = g
                && !g.is_empty()
            {
                o["geometry"] = J::String(geom::to_wkt(&g));
            }
            o
        }
        Value::Tuple(t) => J::Array(t.items.iter().map(|e| json(ctx, &Value::Elem(e.clone()))).collect::<R<Vec<_>>>()?),
        Value::Set(_) | Value::List(_) => {
            // geometry for all elements in one pass
            let items = ctx.items_of(v, Hints { geom: true, ..Default::default() }, knut_lang::diag::Span::default())?;
            ctx.preload_geoms(&items);
            J::Array(items.iter().map(|x| json(ctx, x)).collect::<R<Vec<_>>>()?)
        }
        Value::Group(g) => json!({"key": json(ctx, &g.key)?, "value": json(ctx, &g.value)?, "count": g.count}),
        Value::Grouped(g) => J::Array(
            g.rows
                .iter()
                .map(|r| -> R<J> {
                    let mut o = serde_json::Map::new();
                    match &r.key {
                        Value::List(ks) if g.key_names.len() > 1 => {
                            for (n, k) in g.key_names.iter().zip(ks.iter()) {
                                o.insert(n.clone(), json(ctx, k)?);
                            }
                        }
                        k => {
                            o.insert(g.key_names.first().cloned().unwrap_or_else(|| "key".into()), json(ctx, k)?);
                        }
                    }
                    o.insert(g.value_name.clone(), json(ctx, &r.value)?);
                    Ok(J::Object(o))
                })
                .collect::<R<Vec<_>>>()?,
        ),
        // an isochrone as its outline, not cell by cell (the same area, a fraction of the size)
        Value::Region(r) => J::String(geom::to_wkt(&r.display_geom())),
        Value::Path(p) => json!({"length_m": p.length, "duration_s": p.duration, "ways": p.ways, "geometry": geom::to_wkt(&p.geom)}),
        Value::Record(r) => J::Object(r.iter().map(|(k, x)| Ok((k.clone(), json(ctx, x)?))).collect::<R<serde_json::Map<_, _>>>()?),
        Value::Net(n) => json!({"network_edges": n.n_edges()}),
        Value::Lambda(_) => J::Null,
    })
}

pub fn parquet(ctx: &Ctx, v: &Value, path: &std::path::Path) -> R<()> {
    use arrow_array::{ArrayRef, Float64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    let t = table(ctx, v)?;
    let mut fields = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    for (i, c) in t.cols.iter().enumerate() {
        let numeric = t.rows.iter().all(|r| matches!(r.get(i), Some(Value::Num(_)) | Some(Value::None) | None));
        if numeric {
            let unit = t.rows.iter().find_map(|r| if let Some(Value::Num(n)) = r.get(i) { Some(n.unit_name()) } else { None }).unwrap_or_default();
            let name = if unit.is_empty() { c.clone() } else { format!("{c} [{unit}]") };
            fields.push(Field::new(name, DataType::Float64, true));
            arrays.push(Arc::new(Float64Array::from(
                t.rows.iter().map(|r| if let Some(Value::Num(n)) = r.get(i) { Some(n.shown()) } else { None }).collect::<Vec<_>>(),
            )));
        } else {
            fields.push(Field::new(c.clone(), DataType::Utf8, true));
            arrays.push(Arc::new(StringArray::from(
                t.rows.iter().map(|r| r.get(i).filter(|x| !x.is_none()).map(plain)).collect::<Vec<_>>(),
            )));
        }
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|e| RtErr::internal(knut_lang::diag::Span::default(), e))?;
    let f = std::fs::File::create(path).map_err(|e| RtErr::internal(knut_lang::diag::Span::default(), e))?;
    let mut w = parquet::arrow::ArrowWriter::try_new(f, schema, None).map_err(|e| RtErr::internal(knut_lang::diag::Span::default(), e))?;
    w.write(&batch).map_err(|e| RtErr::internal(knut_lang::diag::Span::default(), e))?;
    w.close().map_err(|e| RtErr::internal(knut_lang::diag::Span::default(), e))?;
    Ok(())
}

/// True if a result is empty (W002).
pub fn is_empty(ctx: &Ctx, v: &Value) -> bool {
    match v {
        Value::Set(s) => s.items(ctx, Hints::default()).map(|x| x.is_empty()).unwrap_or(false),
        Value::List(l) => l.is_empty(),
        Value::Grouped(g) => g.rows.is_empty(),
        _ => false,
    }
}
