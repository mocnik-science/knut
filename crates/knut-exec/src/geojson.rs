//! Results as GeoJSON for maps (`knut web`).
//!
//! Elements become features with their tags and `@id`, `@type`, `@version`, `@time`.
//! Groups keyed by areas, elements or tiles become features with the group value in
//! `@value` (choropleths). Regions, isochrones and paths become one feature each.

use crate::eval::Ctx;
use crate::geom::{Geom, P};
use crate::set::Hints;
use crate::value::*;
use knut_lang::dates;
use knut_lang::diag::Span;
use serde_json::{Map, Value as J, json};

/// A feature collection and whether it was cut at `max` features.
pub struct Features {
    pub collection: J,
    pub count: usize,
    pub truncated: bool,
}

fn pt(p: &P) -> J {
    json!([round(p.0), round(p.1)])
}

/// 7 decimals (≈ 1 cm) keep the files small.
fn round(x: f64) -> f64 {
    (x * 1e7).round() / 1e7
}

pub fn geometry(g: &Geom) -> Option<J> {
    Some(match g {
        Geom::Empty => return None,
        Geom::Point(p) => json!({"type": "Point", "coordinates": pt(p)}),
        Geom::Line(l) if l.len() >= 2 => json!({"type": "LineString", "coordinates": l.iter().map(pt).collect::<Vec<_>>()}),
        Geom::Line(l) => json!({"type": "Point", "coordinates": pt(l.first()?)}),
        Geom::Poly(ps) => {
            let polys: Vec<J> = ps
                .iter()
                .map(|p| J::Array(std::iter::once(&p.outer).chain(&p.holes).map(|r| J::Array(r.iter().map(pt).collect())).collect()))
                .collect();
            match polys.len() {
                0 => return None,
                1 => json!({"type": "Polygon", "coordinates": polys[0]}),
                _ => json!({"type": "MultiPolygon", "coordinates": polys}),
            }
        }
        Geom::Multi(gs) => {
            let gs: Vec<J> = gs.iter().filter_map(geometry).collect();
            if gs.is_empty() {
                return None;
            }
            json!({"type": "GeometryCollection", "geometries": gs})
        }
    })
}

/// Polygon of the Web Mercator tile `z/x/y`.
fn tile_polygon(name: &str) -> Option<Geom> {
    let mut it = name.split('/').map(|s| s.parse::<f64>());
    let (z, x, y) = (it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?);
    let n = 2f64.powf(z);
    let lon = |x: f64| x / n * 360.0 - 180.0;
    let lat = |y: f64| (std::f64::consts::PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
    let (x0, x1, y0, y1) = (lon(x), lon(x + 1.0), lat(y + 1.0), lat(y));
    Some(Geom::Poly(vec![crate::geom::Polygon { outer: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)], holes: vec![] }]))
}

fn scalar(v: &Value) -> J {
    match v {
        Value::None => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Num(n) => json!(n.shown()),
        Value::Text(s) => J::String(s.to_string()),
        Value::Tag(t) => J::String(t.text.to_string()),
        Value::Time(d) => J::String(dates::format(d)),
        other => J::String(other.display()),
    }
}

fn elem_props(e: &Elem) -> Map<String, J> {
    let mut p = Map::new();
    p.insert("@type".into(), json!(e.otype.map_or(e.kind.name(), |o| o.name())));
    p.insert("@kind".into(), json!(e.kind.name()));
    p.insert("@id".into(), json!(e.id));
    if e.v.version > 0 {
        p.insert("@version".into(), json!(e.v.version));
        p.insert("@time".into(), json!(dates::format_instant(e.v.ts)));
    }
    p.insert("@label".into(), json!(e.label()));
    for (k, v) in e.v.tags.iter() {
        p.insert(k.to_string(), json!(v.as_ref()));
    }
    p
}

struct Builder<'a> {
    ctx: &'a Ctx,
    max: usize,
    out: Vec<J>,
    truncated: bool,
}

impl Builder<'_> {
    fn push(&mut self, g: &Geom, props: Map<String, J>) {
        if self.out.len() >= self.max {
            self.truncated = true;
            return;
        }
        if let Some(geometry) = geometry(g) {
            self.out.push(json!({"type": "Feature", "geometry": geometry, "properties": props}));
        }
    }

    fn elem(&mut self, e: &Elem, extra: Option<(&str, J)>) {
        let g = self.ctx.elem_geom(e);
        let mut p = elem_props(e);
        if let Some((k, v)) = extra {
            p.insert(k.into(), v);
        }
        // hits of geocoding: the address and its parts, and the score or distance
        if let Extra::Hit { score, distance } = &e.extra {
            match distance {
                Some(d) => p.insert("@distance".into(), json!(format!("{} m", crate::value::fmt_f64((d * 10.0).round() / 10.0)))),
                None => p.insert("@score".into(), json!(score)),
            };
            let v = Value::Elem(std::sync::Arc::new(Elem::new(e.kind, e.otype, e.id, e.v.clone(), e.at)));
            for f in ["address", "suburb", "city", "state", "country"] {
                if let Ok(Some(Value::Text(t))) = crate::address::field(self.ctx, &v, e, f) {
                    p.insert(format!("@{f}"), json!(t.as_ref()));
                }
            }
        }
        self.push(&g, p);
    }

    fn value(&mut self, v: &Value) -> R<()> {
        match v {
            Value::Elem(e) => self.elem(e, None),
            Value::Tuple(t) => {
                // a sequence match is shown at its last element
                if let Some(e) = t.items.last() {
                    let seq: Vec<String> = t.items.iter().map(|x| format!("v{}", x.v.version)).collect();
                    self.elem(e, Some(("@sequence", json!(seq.join(" → ")))));
                }
            }
            Value::Set(_) | Value::List(_) => {
                let items = self.ctx.items_of(v, Hints { geom: true, ..Default::default() }, Span::default())?;
                self.ctx.preload_geoms(&items[..items.len().min(self.max + 1)]);
                for it in items.iter() {
                    if self.out.len() >= self.max {
                        self.truncated = true;
                        break;
                    }
                    self.value(it)?;
                }
            }
            Value::Group(g) => self.group(&g.key, &g.value)?,
            Value::Grouped(g) => {
                for r in &g.rows {
                    self.group(&r.key, &r.value)?;
                }
            }
            Value::Region(r) => {
                let mut p = Map::new();
                p.insert("@label".into(), json!(v.display()));
                self.push(&r.display_geom(), p);
            }
            Value::Path(p) => {
                let mut m = Map::new();
                m.insert("@label".into(), json!("path"));
                m.insert("length_m".into(), json!(p.length));
                m.insert("duration_s".into(), json!(p.duration));
                self.push(&p.geom, m);
            }
            _ => {}
        }
        Ok(())
    }

    /// A group with a spatial key: the key's geometry carrying the group's value.
    fn group(&mut self, key: &Value, value: &Value) -> R<()> {
        let keys: Vec<&Value> = match key {
            Value::List(ks) => ks.iter().collect(),
            k => vec![k],
        };
        let label = keys.iter().map(|k| k.display()).collect::<Vec<_>>().join(", ");
        let val = match value {
            Value::Set(_) | Value::List(_) => json!(self.ctx.items_of(value, Hints::default(), Span::default())?.len()),
            other => scalar(other),
        };
        for k in keys {
            let g = match k {
                Value::Elem(e) => Some((*self.ctx.elem_geom(e)).clone()),
                Value::Region(r) => Some(r.display_geom()),
                Value::Text(t) => tile_polygon(t),
                _ => None,
            };
            if let Some(g) = g {
                let mut p = Map::new();
                p.insert("@label".into(), json!(label));
                p.insert("@value".into(), val.clone());
                self.push(&g, p);
                return Ok(());
            }
        }
        // groups of elements without a spatial key: show the elements
        if let Value::Set(_) | Value::List(_) = value {
            self.value(value)?;
        }
        Ok(())
    }
}

/// GeoJSON of a result, or `None` if it has nothing to draw.
pub fn features(ctx: &Ctx, v: &Value, max: usize) -> R<Option<Features>> {
    let mut b = Builder { ctx, max, out: Vec::new(), truncated: false };
    b.value(v)?;
    if b.out.is_empty() {
        return Ok(None);
    }
    let count = b.out.len();
    Ok(Some(Features { collection: json!({"type": "FeatureCollection", "features": b.out}), count, truncated: b.truncated }))
}
