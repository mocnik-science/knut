//! Per-run caches: spatial indexes, networks, place names, side indexes, data sets.

use crate::cs::CsIndex;
use crate::net::Net;
use crate::notes::NotesIndex;
use crate::set::SetV;
use crate::spatial::{GeoIndex, NetNearest};
use crate::value::{R, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// A value built once and then shared. Building never blocks other threads: work stealing
/// in rayon could make a waiting thread wait for itself. Sets that conditions need are
/// prepared before parallel scans (`Ctx::warm_up`), so duplicate builds are rare.
pub type Slot<T> = Arc<OnceLock<R<Arc<T>>>>;

/// The value of a slot, built with `f` if it is not there yet.
pub fn get_or_build<T>(slot: &Slot<T>, f: impl FnOnce() -> R<Arc<T>>) -> R<Arc<T>> {
    if let Some(v) = slot.get() {
        return v.clone();
    }
    let v = f();
    let _ = slot.set(v);
    slot.get().expect("set").clone()
}

/// The slot for `key`, created empty if there is none.
pub fn slot<K: std::hash::Hash + Eq, T>(m: &Mutex<HashMap<K, Slot<T>>>, key: K) -> Slot<T> {
    m.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().clone()
}

#[derive(Default)]
pub struct Caches {
    /// Spatial indexes by set (the set is kept so that its address stays unique).
    pub geo: Mutex<HashMap<usize, (Arc<SetV>, Slot<GeoIndex>)>>,
    pub nets: Mutex<HashMap<String, Slot<Net>>>,
    pub net_nearest: Mutex<HashMap<(usize, u8, u8), (Arc<SetV>, Slot<NetNearest>)>>,
    pub places: Mutex<HashMap<String, R<Value>>>,
    pub cs: OnceLock<R<Arc<CsIndex>>>,
    pub notes: OnceLock<R<Arc<NotesIndex>>>,
    pub keys: Mutex<HashMap<i64, Value>>,
    pub datasets: Mutex<HashMap<String, R<Value>>>,
    /// Values of sub-expressions that do not depend on the element being evaluated
    /// (`near some _ in (areas …)` inside a condition), by span.
    pub consts: Mutex<HashMap<knut_lang::diag::Span, Value>>,
    /// Whether a sub-expression is closed in that sense, by span.
    pub closed: Mutex<HashMap<knut_lang::diag::Span, bool>>,
    /// Messages for the user about slow one-time work (index builds).
    pub notices: Mutex<Vec<String>>,
}
