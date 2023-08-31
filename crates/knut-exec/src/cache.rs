//! Per-run caches: spatial indexes, networks, place names, side indexes, data sets.

use crate::cs::CsIndex;
use crate::net::Net;
use crate::notes::NotesIndex;
use crate::set::SetV;
use crate::spatial::{GeoIndex, NetNearest};
use crate::value::{R, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
pub struct Caches {
    pub geo: Mutex<Vec<(Arc<SetV>, Arc<GeoIndex>)>>,
    pub nets: Mutex<HashMap<String, Arc<Net>>>,
    #[allow(clippy::type_complexity)]
    pub net_nearest: Mutex<HashMap<(usize, u8, u8), (Arc<SetV>, Arc<NetNearest>)>>,
    pub places: Mutex<HashMap<String, R<Value>>>,
    pub cs: OnceLock<R<Arc<CsIndex>>>,
    pub notes: OnceLock<R<Arc<NotesIndex>>>,
    pub keys: Mutex<HashMap<i64, Value>>,
    pub datasets: Mutex<HashMap<String, R<Value>>>,
    /// Messages for the user about slow one-time work (index builds).
    pub notices: Mutex<Vec<String>>,
}
