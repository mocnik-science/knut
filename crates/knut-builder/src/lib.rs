//! knut builder: converts OSM history PBF (`*.osh.pbf`) into `.knutdb` files.
//!
//! Pipeline (SPEC.md §1, spec task §11):
//! 1. ways + relations → node classification (private / shared)
//! 2. nodes → node store (referenced nodes), shared node segments
//! 3. ways → way segments incl. private node time series
//! 4. relations → version boxes (iteratively resolved), relation segments
//! 5. sort by (section, hilbert, id), build blocks, train zstd dictionaries, write

pub mod chunked;
pub mod extsort;
pub mod pbf;
pub mod rawcols;
pub mod segment;
pub mod store;
pub mod strcount;
mod writer;

use knut_format::{BBox, Member, ObjType, Segment, Str, Tags};
use rayon::prelude::*;
use segment::{RelBase, SegParams, WaySumsWriter};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Instant;
use extsort::ExtSorter;
use store::{NodeStoreWriter, StoreEntry};

// ---------------------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub struct BuildError(pub String);

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BuildError {}

impl From<knut_format::Error> for BuildError {
    fn from(e: knut_format::Error) -> Self {
        BuildError(e.to_string())
    }
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        BuildError(format!("io: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, BuildError>;

pub fn err(msg: impl Into<String>) -> BuildError {
    BuildError(msg.into())
}

// ---------------------------------------------------------------------------------------
// Input elements

#[derive(Clone, Debug, PartialEq)]
pub struct RawMeta {
    pub version: u32,
    pub timestamp: i64,
    pub changeset: u64,
    pub uid: u32,
    pub user: Str,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawNode {
    pub id: i64,
    pub meta: RawMeta,
    pub tags: Tags,
    pub lon: i32,
    pub lat: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawWay {
    pub id: i64,
    pub meta: RawMeta,
    pub tags: Tags,
    pub refs: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawRel {
    pub id: i64,
    pub meta: RawMeta,
    pub tags: Tags,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Element {
    Node(RawNode),
    Way(RawWay),
    Relation(RawRel),
}

impl Element {
    pub fn obj_type(&self) -> ObjType {
        match self {
            Element::Node(_) => ObjType::Node,
            Element::Way(_) => ObjType::Way,
            Element::Relation(_) => ObjType::Relation,
        }
    }

    pub fn id(&self) -> i64 {
        match self {
            Element::Node(n) => n.id,
            Element::Way(w) => w.id,
            Element::Relation(r) => r.id,
        }
    }

    pub fn meta(&self) -> &RawMeta {
        match self {
            Element::Node(n) => &n.meta,
            Element::Way(w) => &w.meta,
            Element::Relation(r) => &r.meta,
        }
    }
}

/// A history source sorted by type, then id, then version. Each pass may be called
/// several times and yields batches in file order.
pub trait Source: Sync {
    fn nodes(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()>;
    fn ways(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()>;
    fn relations(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()>;

    /// Node ids referenced as relation members (all versions; duplicates allowed).
    fn relation_node_members(&self, f: &mut dyn FnMut(Vec<i64>) -> Result<()>) -> Result<()> {
        self.relations(&mut |batch| {
            let mut v = Vec::new();
            for e in batch {
                if let Element::Relation(r) = e {
                    v.extend(r.members.iter().filter(|m| m.kind == ObjType::Node).map(|m| m.id));
                }
            }
            f(v)
        })
    }
}

/// In-memory source (tests, synthetic data). Elements must be sorted.
pub struct VecSource {
    pub elements: Vec<Element>,
    pub batch: usize,
}

impl VecSource {
    pub fn new(mut elements: Vec<Element>) -> VecSource {
        elements.sort_by_key(|e| (e.obj_type(), e.id(), e.meta().version));
        VecSource { elements, batch: 7 }
    }

    fn run(&self, t: ObjType, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        let v: Vec<Element> = self.elements.iter().filter(|e| e.obj_type() == t).cloned().collect();
        for c in v.chunks(self.batch.max(1)) {
            f(c.to_vec())?;
        }
        Ok(())
    }
}

impl Source for VecSource {
    fn nodes(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        self.run(ObjType::Node, f)
    }
    fn ways(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        self.run(ObjType::Way, f)
    }
    fn relations(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        self.run(ObjType::Relation, f)
    }
}

// ---------------------------------------------------------------------------------------
// Options and statistics

#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Target uncompressed block size in bytes.
    pub block_size: usize,
    pub zstd_level: i32,
    pub zstd_dict: bool,
    pub zstd_dict_size: usize,
    pub keyframe_interval: u32,
    pub strip_users: bool,
    /// Secret used to pseudonymize uids (implies stripping user names).
    pub pseudonymize_uids: Option<String>,
    pub seg: SegParams,
    pub min_value_count: u64,
    pub relation_depth: u32,
    /// Blocks may always grow to this extent (meters) before the compactness rule cuts.
    pub block_min_extent_m: f64,
    /// Sort key: S2-style cube-face cells instead of the lon/lat Hilbert grid (experiment).
    pub s2_sort_key: bool,
    /// Memory budget in bytes for buffers that can spill to disk (pairs, segment rows,
    /// ID index entries). Larger inputs are sorted externally in `tmp_dir`.
    pub mem_budget: usize,
    /// Directory for temporary files (default: next to the output file).
    pub tmp_dir: Option<std::path::PathBuf>,
    /// Order segments by id (instead of Hilbert key) inside each block.
    pub sort_by_id_in_block: bool,
    pub source_name: String,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            block_size: 256 * 1024,
            zstd_level: 19,
            zstd_dict: true,
            zstd_dict_size: 64 * 1024,
            keyframe_interval: 32,
            strip_users: false,
            pseudonymize_uids: None,
            seg: SegParams::default(),
            min_value_count: 16,
            relation_depth: 3,
            block_min_extent_m: 20_000.0,
            sort_by_id_in_block: true,
            mem_budget: default_mem_budget(),
            s2_sort_key: false,
            tmp_dir: None,
            source_name: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BuildStats {
    pub n_objects: [u64; 3],
    pub n_versions: [u64; 3],
    pub n_private_nodes: u64,
    pub n_segments: u64,
    pub n_multi_segment_objects: [u64; 3],
    pub n_large_segments: u64,
    pub n_blocks: u64,
    pub tmin: i64,
    pub tmax: i64,
    pub bbox: BBox,
    pub output_bytes: u64,
    pub section_bytes: Vec<(String, u64)>,
    pub phase_secs: Vec<(String, f64)>,
    pub peak_rss_bytes: u64,
    pub peak_footprint_bytes: u64,
    pub spilled_bytes: u64,
}

impl fmt::Display for BuildStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "objects   nodes={} (private {}) ways={} relations={}",
            self.n_objects[0], self.n_private_nodes, self.n_objects[1], self.n_objects[2])?;
        writeln!(f, "versions  nodes={} ways={} relations={}", self.n_versions[0], self.n_versions[1], self.n_versions[2])?;
        writeln!(f, "segments  {} (large {}), multi-segment objects n={} w={} r={}",
            self.n_segments, self.n_large_segments, self.n_multi_segment_objects[0],
            self.n_multi_segment_objects[1], self.n_multi_segment_objects[2])?;
        writeln!(f, "blocks    {}", self.n_blocks)?;
        writeln!(f, "output    {} bytes", self.output_bytes)?;
        writeln!(f, "spilled   {} MiB (temporary, compressed)", self.spilled_bytes >> 20)?;
        for (k, v) in &self.section_bytes {
            writeln!(f, "  {k:<12} {v:>14}")?;
        }
        for (k, v) in &self.phase_secs {
            writeln!(f, "phase {k:<14} {v:>8.2}s")?;
        }
        writeln!(f, "peak RSS  {:.1} MiB (incl. clean pages of mapped files)", self.peak_rss_bytes as f64 / 1048576.0)?;
        write!(f, "peak footprint {:.1} MiB", self.peak_footprint_bytes as f64 / 1048576.0)
    }
}

/// Default spill budget: 40 % of physical memory (16 GiB if unknown).
pub fn default_mem_budget() -> usize {
    let mut mem: u64 = 0;
    #[cfg(target_os = "macos")]
    // SAFETY: sysctlbyname with a correctly sized out buffer.
    unsafe {
        let mut len = std::mem::size_of::<u64>();
        libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut mem as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: sysconf has no preconditions.
    unsafe {
        mem = libc::sysconf(libc::_SC_PHYS_PAGES) as u64 * libc::sysconf(libc::_SC_PAGESIZE) as u64;
    }
    if mem == 0 { 16 << 30 } else { (mem / 5 * 2) as usize }
}

/// Peak physical footprint (macOS: dirty + anonymous memory, excluding clean file pages of
/// memory-mapped files); falls back to max RSS elsewhere.
pub fn peak_footprint_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    // SAFETY: proc_pid_rusage writes into a correctly typed struct.
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&mut info as *mut libc::rusage_info_v4).cast()) == 0 {
            return info.ri_lifetime_max_phys_footprint;
        }
    }
    peak_rss_bytes()
}

pub fn peak_rss_bytes() -> u64 {
    // SAFETY: getrusage with a valid out-pointer.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return 0;
        }
        if cfg!(target_os = "macos") { ru.ru_maxrss as u64 } else { ru.ru_maxrss as u64 * 1024 }
    }
}

// ---------------------------------------------------------------------------------------
// Counters (dictionaries, users)

#[derive(Default)]
pub(crate) struct Counters {
    pub keys: HashMap<Str, u64>,
    pub values: HashMap<Str, u64>,
    pub roles: HashMap<Str, u64>,
    /// (uid, name) → (first_seen, last_seen)
    pub users: HashMap<(u32, Str), (i64, i64)>,
}

impl Counters {
    fn add(&mut self, m: &RawMeta, tags: &Tags) {
        for (k, v) in tags.iter() {
            *self.keys.entry(k.clone()).or_default() += 1;
            *self.values.entry(v.clone()).or_default() += 1;
        }
        let e = self.users.entry((m.uid, m.user.clone())).or_insert((m.timestamp, m.timestamp));
        e.0 = e.0.min(m.timestamp);
        e.1 = e.1.max(m.timestamp);
    }

    fn merge(mut self, o: Counters) -> Counters {
        let (mut a, b) = if self.values.len() >= o.values.len() { (self, o) } else { (o, self) };
        for (k, v) in b.keys {
            *a.keys.entry(k).or_default() += v;
        }
        for (k, v) in b.values {
            *a.values.entry(k).or_default() += v;
        }
        for (k, v) in b.roles {
            *a.roles.entry(k).or_default() += v;
        }
        for (k, (f, l)) in b.users {
            let e = a.users.entry(k).or_insert((f, l));
            e.0 = e.0.min(f);
            e.1 = e.1.max(l);
        }
        self = a;
        self
    }
}

// ---------------------------------------------------------------------------------------
// Grouping of versions by id across batches

struct Grouper<T> {
    pending: Vec<T>,
    last_id: Option<i64>,
}

impl<T> Grouper<T> {
    fn new() -> Self {
        Grouper { pending: Vec::new(), last_id: None }
    }

    fn feed(&mut self, items: Vec<T>, id: impl Fn(&T) -> i64, version: impl Fn(&T) -> u32) -> Result<Vec<Vec<T>>> {
        let mut out = Vec::new();
        let mut cur = std::mem::take(&mut self.pending);
        for it in items {
            let iid = id(&it);
            if let Some(last) = cur.last() {
                let lid = id(last);
                if lid != iid {
                    if iid < lid {
                        return Err(err(format!("input not sorted by id ({iid} after {lid})")));
                    }
                    out.push(std::mem::take(&mut cur));
                } else if version(&it) < version(last) {
                    return Err(err(format!("versions of object {iid} not sorted")));
                }
            } else if let Some(l) = self.last_id
                && iid < l {
                    return Err(err(format!("input not sorted by id ({iid} after {l})")));
                }
            cur.push(it);
        }
        if let Some(l) = out.last().and_then(|g: &Vec<T>| g.last()) {
            self.last_id = Some(id(l));
        }
        self.pending = cur;
        Ok(out)
    }

    fn finish(self) -> Option<Vec<T>> {
        (!self.pending.is_empty()).then_some(self.pending)
    }
}

fn take_nodes(b: Vec<Element>) -> Vec<RawNode> {
    b.into_iter().filter_map(|e| if let Element::Node(n) = e { Some(n) } else { None }).collect()
}

fn take_ways(b: Vec<Element>) -> Vec<RawWay> {
    b.into_iter().filter_map(|e| if let Element::Way(w) = e { Some(w) } else { None }).collect()
}

fn take_rels(b: Vec<Element>) -> Vec<RawRel> {
    b.into_iter().filter_map(|e| if let Element::Relation(r) = e { Some(r) } else { None }).collect()
}

/// Runs `f` over complete id groups of one pass.
fn for_groups<T: Send>(
    pass: impl FnOnce(&mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()>,
    take: fn(Vec<Element>) -> Vec<T>,
    id: fn(&T) -> i64,
    version: fn(&T) -> u32,
    mut f: impl FnMut(Vec<Vec<T>>) -> Result<()>,
) -> Result<()> {
    let mut g = Grouper::new();
    pass(&mut |batch| {
        let groups = g.feed(take(batch), id, version)?;
        if !groups.is_empty() {
            f(groups)?;
        }
        Ok(())
    })?;
    if let Some(last) = g.finish() {
        f(vec![last])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Build

/// Streaming node classification from pass 1: merges sorted (node, way) pairs with sorted
/// relation node members. `lookup` must be called with ascending ids.
struct ClassIter {
    pairs: extsort::Sorted<(i64, i64)>,
    rel: extsort::Sorted<i64>,
    next_pair: Option<(i64, i64)>,
    next_rel: Option<i64>,
    cur: Option<(i64, i64)>,
}

impl ClassIter {
    fn new(mut pairs: extsort::Sorted<(i64, i64)>, mut rel: extsort::Sorted<i64>) -> Result<Self> {
        let next_pair = pairs.next_rec()?;
        let next_rel = rel.next_rec()?;
        let mut c = ClassIter { pairs, rel, next_pair, next_rel, cur: None };
        c.advance()?;
        Ok(c)
    }

    /// Computes the next classified node: (id, single way id or -1).
    fn advance(&mut self) -> Result<()> {
        let pid = self.next_pair.map_or(i64::MAX, |p| p.0);
        let rid = self.next_rel.unwrap_or(i64::MAX);
        if pid == i64::MAX && rid == i64::MAX {
            self.cur = None;
            return Ok(());
        }
        let id = pid.min(rid);
        let mut w = -1i64;
        if pid == id {
            w = self.next_pair.unwrap().1;
            self.next_pair = self.pairs.next_rec()?;
            while let Some((n, _)) = self.next_pair {
                if n != id {
                    break;
                }
                w = -1;
                self.next_pair = self.pairs.next_rec()?;
            }
        }
        if rid == id {
            w = -1;
            self.next_rel = self.rel.next_rec()?;
        }
        self.cur = Some((id, w));
        Ok(())
    }

    /// `Some(way or -1)` if node `id` is referenced by a way or relation.
    fn lookup(&mut self, id: i64) -> Result<Option<i64>> {
        while let Some((c, _)) = self.cur {
            if c >= id {
                break;
            }
            self.advance()?;
        }
        Ok(match self.cur {
            Some((c, w)) if c == id => Some(w),
            _ => None,
        })
    }
}

pub(crate) struct Collected {
    pub sink: writer::Sink,
    pub counters: Counters,
    pub stats: BuildStats,
}

type Record = (writer::SegKey, BBox, Vec<u8>);

static USE_S2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn record_segments(segs: &[Segment]) -> Result<Vec<Record>> {
    segs.iter()
        .enumerate()
        .map(|(i, s)| {
            let section = if s.flags & knut_format::SEG_LARGE != 0 { 3 } else { 0 } + s.obj_type() as u8;
            let hilbert = if s.bbox.is_empty() {
                u64::MAX
            } else {
                let (x, y) = s.bbox.center();
                if USE_S2.load(std::sync::atomic::Ordering::Relaxed) {
                    knut_format::hilbert::s2_key(x, y)
                } else {
                    knut_format::hilbert::key(x, y)
                }
            };
            let bytes = writer::encode_row(s)?;
            Ok((writer::SegKey { section, hilbert, id: s.id, seg_no: i as u32 }, s.bbox, bytes))
        })
        .collect()
}

fn count_groups<T: Sync>(groups: &[Vec<T>], meta: fn(&T) -> (&RawMeta, &Tags)) -> Counters {
    groups
        .par_iter()
        .fold(Counters::default, |mut c, g| {
            for v in g {
                let (m, t) = meta(v);
                c.add(m, t);
            }
            c
        })
        .reduce(Counters::default, Counters::merge)
}

/// Builds a `.knutdb` file from `src`.
pub fn build(src: &dyn Source, out: &Path, opt: &BuildOptions, log_raw: &(dyn Fn(&str) + Sync)) -> Result<BuildStats> {
    let log = |m: &str| log_raw(&format!("{m} [peak footprint {} MiB]", peak_footprint_bytes() >> 20));
    let p = &opt.seg;
    USE_S2.store(opt.s2_sort_key, std::sync::atomic::Ordering::Relaxed);
    let mut phases: Vec<(String, f64)> = Vec::new();
    let mut stats = BuildStats { tmin: i64::MAX, tmax: i64::MIN, ..Default::default() };

    // ---- pass 1: classification (external sort of (node, way) pairs)
    let t0 = Instant::now();
    let tmp = opt.tmp_dir.clone().unwrap_or_else(|| out.parent().map(Path::to_path_buf).unwrap_or_default());
    std::fs::create_dir_all(&tmp)?;
    let budget = opt.mem_budget;
    let mut pairs = ExtSorter::<(i64, i64)>::new(&tmp, "pairs", budget / 2, true);
    for_groups(|f| src.ways(f), take_ways, |w| w.id, |w| w.meta.version, |groups| {
        let part: Vec<(i64, i64)> = groups
            .par_iter()
            .flat_map_iter(|g| {
                let wid = g[0].id;
                let mut refs: Vec<i64> = g.iter().flat_map(|v| v.refs.iter().copied()).collect();
                refs.sort_unstable();
                refs.dedup();
                refs.into_iter().map(move |n| (n, wid))
            })
            .collect();
        pairs.extend(part)
    })?;
    let mut rel_nodes = ExtSorter::<i64>::new(&tmp, "relnodes", budget / 8, true);
    src.relation_node_members(&mut |ids| rel_nodes.extend(ids))?;
    let pairs_spill = pairs.spilled_bytes;
    stats.spilled_bytes += pairs_spill;
    let mut class = ClassIter::new(pairs.finish()?, rel_nodes.finish()?)?;
    log(&format!(
        "pass 1: pairs sorted ({} MiB spilled) ({:.1}s)",
        pairs_spill >> 20,
        t0.elapsed().as_secs_f64()
    ));
    phases.push(("classify".into(), t0.elapsed().as_secs_f64()));

    let mut sink = writer::Sink::new(&tmp, budget * 7 / 20, opt.block_size);
    let mut counters = Counters::default();
    let mut value_counter = strcount::SpillingCounter::new(&tmp, budget / 12);

    // ---- pass 2: nodes
    let t1 = Instant::now();
    let mut store_w = NodeStoreWriter::new(&tmp)?;
    let mut n_class = 0u64;
    for_groups(|f| src.nodes(f), take_nodes, |n| n.id, |n| n.meta.version, |groups| {
        let c = count_groups(&groups, |n| (&n.meta, &n.tags));
        counters = std::mem::take(&mut counters).merge(c);
        value_counter.maybe_spill(&mut counters.values)?;
        // merge-join with the classification stream (both sorted by node id)
        let mut cls: Vec<Option<i64>> = Vec::with_capacity(groups.len());
        for g in &groups {
            cls.push(class.lookup(g[0].id)?);
        }
        let outs: Vec<Result<(bool, Vec<Record>, usize)>> = groups
            .par_iter()
            .zip(&cls)
            .map(|(g, cl)| {
                let tagged = g.iter().any(|v| !v.tags.is_empty());
                let private = cl.is_some_and(|w| w >= 0) && !tagged;
                let (recs, nseg) = if private {
                    (Vec::new(), 0)
                } else {
                    let segs = segment::segment_node(g[0].id, g, p);
                    (record_segments(&segs)?, segs.len())
                };
                Ok((private, recs, nseg))
            })
            .collect();
        for ((g, o), cl) in groups.iter().zip(outs).zip(&cls) {
            let (private, recs, nseg) = o?;
            if cl.is_some() {
                n_class += 1;
                store_w.push(
                    g[0].id,
                    private,
                    g.iter().map(|v| StoreEntry {
                        t: v.meta.timestamp,
                        lon: v.lon,
                        lat: v.lat,
                        visible: v.meta.visible,
                        version: v.meta.version,
                        changeset: v.meta.changeset,
                        uid: v.meta.uid,
                    }),
                )?;
            }
            stats.n_objects[0] += 1;
            stats.n_versions[0] += g.len() as u64;
            if private {
                stats.n_private_nodes += 1;
            }
            if nseg > 1 {
                stats.n_multi_segment_objects[0] += 1;
            }
            for v in g {
                stats.tmin = stats.tmin.min(v.meta.timestamp);
                stats.tmax = stats.tmax.max(v.meta.timestamp);
            }
            for (k, bb, b) in recs {
                sink.push(k, bb, &b)?;
            }
        }
        Ok(())
    })?;
    drop(class);
    let store = store_w.finish(budget / 8)?;
    log(&format!(
        "pass 2: {} nodes, {} in store ({} private, {} entries, {} MiB compressed on disk) ({:.1}s)",
        stats.n_objects[0],
        store.n_nodes,
        stats.n_private_nodes,
        store.n_entries,
        store.disk_bytes() >> 20,
        t1.elapsed().as_secs_f64()
    ));
    let _ = n_class;
    phases.push(("nodes".into(), t1.elapsed().as_secs_f64()));

    // ---- pass 3: ways
    let t2 = Instant::now();
    let mut way_sums_w = WaySumsWriter::new(&tmp)?;
    for_groups(|f| src.ways(f), take_ways, |w| w.id, |w| w.meta.version, |groups| {
        let c = count_groups(&groups, |w| (&w.meta, &w.tags));
        counters = std::mem::take(&mut counters).merge(c);
        value_counter.maybe_spill(&mut counters.values)?;
        let outs: Vec<Result<(Vec<Segment>, Vec<Record>)>> = groups
            .par_iter()
            .map(|g| {
                let segs = segment::segment_way(g[0].id, g, &store, p);
                let recs = record_segments(&segs)?;
                Ok((segs, recs))
            })
            .collect();
        for (g, o) in groups.iter().zip(outs) {
            let (segs, recs) = o?;
            way_sums_w.push(g[0].id, &segs, p.time_base)?;
            stats.n_objects[1] += 1;
            stats.n_versions[1] += g.len() as u64;
            if segs.len() > 1 {
                stats.n_multi_segment_objects[1] += 1;
            }
            for v in g {
                stats.tmin = stats.tmin.min(v.meta.timestamp);
                stats.tmax = stats.tmax.max(v.meta.timestamp);
            }
            for (k, bb, b) in recs {
                sink.push(k, bb, &b)?;
            }
        }
        Ok(())
    })?;
    let way_sums = way_sums_w.finish(budget / 16)?;
    log(&format!("pass 3: {} ways ({:.1}s)", stats.n_objects[1], t2.elapsed().as_secs_f64()));
    phases.push(("ways".into(), t2.elapsed().as_secs_f64()));

    // ---- pass 4: relation boxes
    let t3 = Instant::now();
    let mut rels: Vec<RelBase> = Vec::new();
    for_groups(|f| src.relations(f), take_rels, |r| r.id, |r| r.meta.version, |groups| {
        let mut c = count_groups(&groups, |r| (&r.meta, &r.tags));
        for g in &groups {
            for v in g {
                for m in &v.members {
                    *c.roles.entry(m.role.clone()).or_default() += 1;
                }
            }
        }
        counters = std::mem::take(&mut counters).merge(c);
        value_counter.maybe_spill(&mut counters.values)?;
        let part: Vec<RelBase> =
            groups.par_iter().map(|g| segment::rel_base(g[0].id, g, &store, &way_sums)).collect();
        rels.extend(part);
        Ok(())
    })?;
    segment::resolve_relations(&mut rels, opt.relation_depth);

    // ---- pass 5: relation segments
    for_groups(|f| src.relations(f), take_rels, |r| r.id, |r| r.meta.version, |groups| {
        let outs: Vec<Result<(usize, Vec<Record>)>> = groups
            .par_iter()
            .map(|g| {
                let id = g[0].id;
                let ri = rels.binary_search_by_key(&id, |r| r.id).map_err(|_| err("relation summary missing"))?;
                let segs = segment::segment_rel(id, g, &rels[ri].boxes, p);
                Ok((segs.len(), record_segments(&segs)?))
            })
            .collect();
        for (g, o) in groups.iter().zip(outs) {
            let (nseg, recs) = o?;
            stats.n_objects[2] += 1;
            stats.n_versions[2] += g.len() as u64;
            if nseg > 1 {
                stats.n_multi_segment_objects[2] += 1;
            }
            for v in g {
                stats.tmin = stats.tmin.min(v.meta.timestamp);
                stats.tmax = stats.tmax.max(v.meta.timestamp);
            }
            for (k, bb, b) in recs {
                sink.push(k, bb, &b)?;
            }
        }
        Ok(())
    })?;
    drop(rels);
    drop(store);
    drop(way_sums);
    log(&format!("pass 4/5: {} relations ({:.1}s)", stats.n_objects[2], t3.elapsed().as_secs_f64()));
    phases.push(("relations".into(), t3.elapsed().as_secs_f64()));

    if stats.tmin > stats.tmax {
        stats.tmin = 0;
        stats.tmax = 0;
    }
    stats.phase_secs = phases;
    stats.spilled_bytes += value_counter.spilled_bytes;
    let values = std::mem::take(&mut counters.values);
    counters.values = value_counter.finish(values, opt.min_value_count.max(1))?;
    let collected = Collected { sink, counters, stats };
    writer::write(collected, out, opt, &log)
}
