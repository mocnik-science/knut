//! Python bindings: `knut._knut`.

use knut_format::time::parse_time;
use knut_format::{BBox, ObjType, e7_to_deg};
use knut::{Feature, Filter, OpenOptions, Reader};
use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::Arc;

fn err(e: impl std::fmt::Display) -> PyErr {
    PyIOError::new_err(e.to_string())
}

fn obj_type(s: &str) -> PyResult<ObjType> {
    ObjType::parse(s).ok_or_else(|| PyValueError::new_err(format!("unknown object type {s:?}")))
}

/// Accepts int (Unix seconds), str (ISO 8601) or datetime.
fn time_arg(v: &PyAny) -> PyResult<i64> {
    if let Ok(i) = v.extract::<i64>() {
        return Ok(i);
    }
    if let Ok(s) = v.extract::<String>() {
        return parse_time(&s).ok_or_else(|| PyValueError::new_err(format!("cannot parse time {s:?}")));
    }
    if v.hasattr("timestamp")? {
        let f: f64 = v.call_method0("timestamp")?.extract()?;
        return Ok(f.floor() as i64);
    }
    Err(PyValueError::new_err("time must be int, str or datetime"))
}

/// pyarrow schema of `ArrowResult.to_arrow()`, matching the dicts of `to_pylist()`.
const ARROW_SCHEMA: &str = "pa.schema([
    ('type', pa.string()),
    ('id', pa.int64()),
    ('version', pa.int32()),
    ('timestamp', pa.timestamp('s', tz='UTC')),
    ('changeset', pa.int64()),
    ('uid', pa.int64()),
    ('visible', pa.bool_()),
    ('tags', pa.map_(pa.string(), pa.string())),
    ('lon', pa.float64()),
    ('lat', pa.float64()),
    ('coords', pa.list_(pa.struct([('lon', pa.float64()), ('lat', pa.float64())]))),
    ('nodes', pa.list_(pa.int64())),
    ('members', pa.list_(pa.struct([('type', pa.string()), ('ref', pa.int64()), ('role', pa.string())]))),
])";

fn bbox_arg(b: Option<(f64, f64, f64, f64)>) -> BBox {
    match b {
        Some((a, c, d, e)) => BBox::from_deg(a, c, d, e),
        None => BBox::WORLD,
    }
}

/// Query result: a list of features.
#[pyclass(module = "knut._knut")]
pub struct ArrowResult {
    features: Arc<Vec<Feature>>,
}

#[pymethods]
impl ArrowResult {
    fn __len__(&self) -> usize {
        self.features.len()
    }

    /// Converts to a ``pyarrow.Table`` (requires pyarrow ≥ 12).
    fn to_arrow<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let rows = self.to_pylist(py)?;
        let locals = PyDict::new(py);
        locals.set_item("pa", py.import("pyarrow")?)?;
        // fixed schema: without it pyarrow takes the columns from the first row only
        let schema = py.eval(ARROW_SCHEMA, None, Some(locals))?;
        py.import("pyarrow")?.getattr("Table")?.call_method1("from_pylist", (rows, schema))
    }

    /// Converts to a list of dicts (no dependencies).
    fn to_pylist<'py>(&self, py: Python<'py>) -> PyResult<&'py PyList> {
        let out = PyList::empty(py);
        for f in self.features.iter() {
            let d = PyDict::new(py);
            d.set_item("type", f.obj_type.name())?;
            d.set_item("id", f.id)?;
            d.set_item("version", f.meta.version)?;
            d.set_item("timestamp", f.meta.timestamp)?;
            d.set_item("changeset", f.meta.changeset)?;
            d.set_item("uid", f.meta.uid)?;
            d.set_item("visible", f.meta.visible)?;
            let tags = PyDict::new(py);
            for (k, v) in f.tags.iter() {
                tags.set_item(k.as_ref(), v.as_ref())?;
            }
            d.set_item("tags", tags)?;
            match &f.geometry {
                knut::Geometry::Point(x, y) => {
                    d.set_item("lon", e7_to_deg(*x))?;
                    d.set_item("lat", e7_to_deg(*y))?;
                }
                knut::Geometry::Line(pts) => {
                    let c: Vec<Option<(f64, f64)>> =
                        pts.iter().map(|p| p.map(|(x, y)| (e7_to_deg(x), e7_to_deg(y)))).collect();
                    d.set_item("coords", c)?;
                }
                knut::Geometry::None => {}
            }
            if f.obj_type == ObjType::Way {
                d.set_item("nodes", f.nodes.clone())?;
            }
            if f.obj_type == ObjType::Relation {
                let m: Vec<(&str, i64, String)> =
                    f.members.iter().map(|m| (m.kind.name(), m.id, m.role.to_string())).collect();
                d.set_item("members", m)?;
            }
            out.append(d)?;
        }
        Ok(out)
    }

    fn __repr__(&self) -> String {
        format!("<knut.ArrowResult rows={}>", self.features.len())
    }
}

/// An open `.knutdb` file.
#[pyclass(module = "knut._knut", frozen)]
pub struct Knut {
    r: Arc<Reader>,
    path: String,
}

fn result(f: Vec<Feature>) -> ArrowResult {
    ArrowResult { features: Arc::new(f) }
}

#[allow(clippy::too_many_arguments)]
fn make_filter(
    types: Option<Vec<String>>,
    key: Option<String>,
    value: Option<String>,
    uid: Option<u32>,
    changeset: Option<u64>,
    include_private_nodes: bool,
    geometry: bool,
) -> PyResult<Filter> {
    let mut f = Filter::default();
    if let Some(ts) = types {
        let t: Vec<ObjType> = ts.iter().map(|s| obj_type(s)).collect::<PyResult<_>>()?;
        f = f.types(&t);
    }
    if let Some(k) = key {
        f = f.tag(&k, value.as_deref());
    } else if value.is_some() {
        return Err(PyValueError::new_err("value requires key"));
    }
    f.uid = uid;
    f.changeset = changeset;
    f.include_private_nodes = include_private_nodes;
    f.geometry = geometry;
    Ok(f)
}

#[pymethods]
impl Knut {
    #[new]
    #[pyo3(signature = (path, verify_checksums=true, cache_blocks=1024))]
    fn new(py: Python<'_>, path: String, verify_checksums: bool, cache_blocks: usize) -> PyResult<Knut> {
        let r = py.allow_threads(|| Reader::open_with(&path, OpenOptions { verify_checksums, cache_blocks })).map_err(err)?;
        Ok(Knut { r: Arc::new(r), path })
    }

    /// Header information as a dict.
    fn info<'py>(&self, py: Python<'py>) -> PyResult<&'py PyDict> {
        let h = &self.r.header;
        let d = PyDict::new(py);
        d.set_item("format", format!("{}.{}", h.format_major, h.format_minor))?;
        d.set_item("features", h.features)?;
        d.set_item("time_range", (h.tmin, h.tmax))?;
        d.set_item(
            "bbox",
            (e7_to_deg(h.bbox.minx), e7_to_deg(h.bbox.miny), e7_to_deg(h.bbox.maxx), e7_to_deg(h.bbox.maxy)),
        )?;
        d.set_item("objects", (h.n_objects[0], h.n_objects[1], h.n_objects[2]))?;
        d.set_item("versions", (h.n_versions[0], h.n_versions[1], h.n_versions[2]))?;
        d.set_item("segments", h.n_segments)?;
        d.set_item("blocks", h.n_blocks)?;
        d.set_item("meta", self.r.meta_text())?;
        Ok(d)
    }

    /// All versions of an object.
    fn get(&self, py: Python<'_>, r#type: &str, id: i64) -> PyResult<ArrowResult> {
        let t = obj_type(r#type)?;
        let r = self.r.clone();
        Ok(result(py.allow_threads(|| r.get(t, id)).map_err(err)?))
    }

    /// The version valid at `time` (empty result if none).
    fn at(&self, py: Python<'_>, r#type: &str, id: i64, time: &PyAny) -> PyResult<ArrowResult> {
        let (t, ts) = (obj_type(r#type)?, time_arg(time)?);
        let r = self.r.clone();
        Ok(result(py.allow_threads(|| r.at(t, id, ts)).map_err(err)?.into_iter().collect()))
    }

    /// Objects within `bbox` (lon_min, lat_min, lon_max, lat_max) at `time`.
    #[pyo3(signature = (bbox, time, key=None, value=None, types=None, uid=None, changeset=None, include_private_nodes=true, geometry=true))]
    #[allow(clippy::too_many_arguments)]
    fn snapshot(
        &self,
        py: Python<'_>,
        bbox: Option<(f64, f64, f64, f64)>,
        time: &PyAny,
        key: Option<String>,
        value: Option<String>,
        types: Option<Vec<String>>,
        uid: Option<u32>,
        changeset: Option<u64>,
        include_private_nodes: bool,
        geometry: bool,
    ) -> PyResult<ArrowResult> {
        let ts = time_arg(time)?;
        let f = make_filter(types, key, value, uid, changeset, include_private_nodes, geometry)?;
        let (r, b) = (self.r.clone(), bbox_arg(bbox));
        Ok(result(py.allow_threads(|| r.snapshot(&b, ts, &f)).map_err(err)?))
    }

    /// All versions with `t0 <= timestamp <= t1` within `bbox`.
    #[pyo3(signature = (bbox, t0, t1, key=None, value=None, types=None, uid=None, changeset=None, include_private_nodes=true, geometry=true))]
    #[allow(clippy::too_many_arguments)]
    fn changes(
        &self,
        py: Python<'_>,
        bbox: Option<(f64, f64, f64, f64)>,
        t0: &PyAny,
        t1: &PyAny,
        key: Option<String>,
        value: Option<String>,
        types: Option<Vec<String>>,
        uid: Option<u32>,
        changeset: Option<u64>,
        include_private_nodes: bool,
        geometry: bool,
    ) -> PyResult<ArrowResult> {
        let (a, z) = (time_arg(t0)?, time_arg(t1)?);
        let f = make_filter(types, key, value, uid, changeset, include_private_nodes, geometry)?;
        let (r, b) = (self.r.clone(), bbox_arg(bbox));
        Ok(result(py.allow_threads(|| r.changes(&b, a, z, &f)).map_err(err)?))
    }

    /// Way geometry at `time`: list of (node_id, lon, lat) (lon/lat None if unknown), or None.
    fn way_geometry_at(
        &self,
        py: Python<'_>,
        id: i64,
        time: &PyAny,
    ) -> PyResult<Option<Vec<(i64, Option<f64>, Option<f64>)>>> {
        let ts = time_arg(time)?;
        let r = self.r.clone();
        let g = py.allow_threads(|| r.way_geometry_at(id, ts)).map_err(err)?;
        Ok(g.map(|g| g.into_iter().map(|(n, p)| (n, p.map(|q| e7_to_deg(q.0)), p.map(|q| e7_to_deg(q.1)))).collect()))
    }

    /// All geometry states of a way: list of (timestamp, [(node_id, lon, lat), ...]).
    #[allow(clippy::type_complexity)]
    fn way_geometry_changes(
        &self,
        py: Python<'_>,
        id: i64,
    ) -> PyResult<Vec<(i64, Vec<(i64, Option<f64>, Option<f64>)>)>> {
        let r = self.r.clone();
        let g = py.allow_threads(|| r.way_geometry_changes(id)).map_err(err)?;
        Ok(g.into_iter()
            .map(|(t, g)| (t, g.into_iter().map(|(n, p)| (n, p.map(|q| e7_to_deg(q.0)), p.map(|q| e7_to_deg(q.1)))).collect()))
            .collect())
    }

    /// Verifies section checksums (and every block if `deep`).
    #[pyo3(signature = (deep=false))]
    fn verify(&self, py: Python<'_>, deep: bool) -> PyResult<(u64, u64)> {
        let r = self.r.clone();
        let rep = py.allow_threads(|| r.verify(deep)).map_err(err)?;
        Ok((rep.blocks, rep.segments))
    }
    /// Path of the opened file.
    #[getter]
    fn path(&self) -> &str {
        &self.path
    }
}

/// Runs a knut script and returns `(exit code, JSON outputs, messages)`. The database
/// comes from `db` or from `knut_db` in the script (relative to `base_dir`).
#[pyfunction]
#[pyo3(signature = (script, db=None, base_dir=None, file=None))]
fn run_script(py: Python<'_>, script: String, db: Option<String>, base_dir: Option<String>, file: Option<String>) -> (i32, String, String) {
    py.allow_threads(|| {
        let opts = knut_exec::Options { db: db.map(Into::into), format: knut_exec::Format::Json, timing: false, ..Default::default() };
        let base = std::path::PathBuf::from(base_dir.unwrap_or_else(|| ".".into()));
        let (mut out, mut errs) = (Vec::new(), Vec::new());
        let code = knut_exec::run_source(&script, file.as_deref(), &base, &opts, &mut out, &mut errs);
        (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&errs).into_owned())
    })
}

#[pymodule]
fn _knut(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run_script, m)?)?;
    m.add_class::<Knut>()?;
    m.add_class::<ArrowResult>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
