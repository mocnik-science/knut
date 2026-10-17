//! What a long-running process (`knut web`) loads before the first query, and what it
//! leaves to the queries. The parts of a database fall into three categories:
//!
//! - **a — always needed**: what practically every query reads (dictionaries, block table,
//!   spatial index, tag filters). Always loaded.
//! - **b — needed by some kinds of query**: the optional indexes (place, name, address,
//!   boundaries, tag statistics, changesets, routing network and hierarchies), the ID index,
//!   the user names, the notes; and the data blocks themselves. Loaded on first use, or
//!   before, depending on the mode.
//! - **c — derived, never stored**: what is computed from a and b and then kept (connected
//!   parts of the routing network, the entries of the boundary index, the map view).
//!   Computed on first use, or before, depending on the mode.
//!
//! The data blocks need no loading of their own: the file is mapped into memory, and only
//! the blocks a query reads are read from disk (the spatial index finds them); the operating
//! system keeps them as long as memory allows, and the decoded ones stay in a cache of the
//! reader. A query about Bremen in a database of the whole world reads the blocks of Bremen.
//!
//! Indexes in the mapped file cost no memory of the process when loaded ahead (only pages of
//! the file, which the system can drop); decoded ones (routing network, hierarchies,
//! changesets, notes) are copies in memory and count against a budget: a share of the
//! physical memory, so that a large database on a small computer loads what fits, the most
//! useful first, and leaves the rest to the queries.

use crate::db::Db;
use knut_format::container::ElementKind;
use std::time::Instant;

/// How much a process loads ahead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Only category a; everything else on first use.
    Minimal,
    /// Category a, and of b and c what is worth it: what is expensive to load on first use
    /// (decoded indexes, connected parts) and the smaller mapped indexes, within half the
    /// physical memory.
    #[default]
    Preload,
    /// Everything, the data blocks too, within most of the physical memory.
    PreloadAll,
}

impl Mode {
    /// The share of the physical memory that loading ahead may fill.
    fn share(self) -> f64 {
        match self {
            Mode::Minimal => 0.0,
            Mode::Preload => 0.5,
            Mode::PreloadAll => 0.85,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Always,
    OnDemand,
    Derived,
}

impl Category {
    pub fn letter(self) -> &'static str {
        match self {
            Category::Always => "a",
            Category::OnDemand => "b",
            Category::Derived => "c",
        }
    }
}

/// The category of an element of the container.
pub fn category(kind: ElementKind) -> Category {
    use ElementKind::*;
    match kind {
        Metadata | Dictionaries | CompressionDictionaries | BlockTable | SpatialIndex | TagFilters => Category::Always,
        _ => Category::OnDemand,
    }
}

/// What happened to one part.
pub struct Step {
    pub name: String,
    pub category: Category,
    /// Bytes it occupies (estimated for decoded parts).
    pub bytes: u64,
    pub seconds: f64,
    /// `None` if loaded, else why not.
    pub skipped: Option<String>,
}

/// The physical memory of the computer, or `KNUT_MEMORY` (bytes, or with G or M: `8G`) to
/// try the budget of a smaller one.
pub fn physical_memory() -> Option<u64> {
    if let Ok(v) = std::env::var("KNUT_MEMORY") {
        let v = v.trim().to_ascii_uppercase();
        let (num, mul) = match v.strip_suffix('G') {
            Some(n) => (n, 1u64 << 30),
            None => match v.strip_suffix('M') {
                Some(n) => (n, 1 << 20),
                None => (v.as_str(), 1),
            },
        };
        if let Ok(n) = num.trim().parse::<f64>() {
            return Some((n * mul as f64) as u64);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: a plain sysctl read into a u64 of the size given.
        let r = unsafe { libc::sysctlbyname(name.as_ptr(), &mut v as *mut u64 as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0) };
        (r == 0 && v > 0).then_some(v)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // SAFETY: sysconf only reads system constants.
        let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
        (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
    }
}

/// Reads one byte of every page: the mapped part is then in memory.
fn touch(b: &[u8]) {
    use rayon::prelude::*;
    let x = b.par_chunks(64 << 20).map(|c| c.iter().step_by(4096).fold(0u8, |a, &v| a ^ v)).reduce(|| 0, |a, b| a ^ b);
    std::hint::black_box(x);
}

fn element_size(db: &Db, kind: ElementKind) -> Option<u64> {
    db.reader.element(kind).map(|el| el.sections.iter().map(|s| s.length).sum())
}

/// What a part needs to be loaded.
enum Part {
    /// Pages of an element of the mapped file.
    Mapped(ElementKind),
    /// A decoded copy in memory: (estimated bytes per byte of the element, load).
    Decoded(ElementKind, f64, fn(&Db)),
    /// Derived from other parts: (name, estimated bytes, compute).
    Derived(&'static str, fn(&Db) -> u64, fn(&Db)),
}

fn load_net(db: &Db) {
    crate::routing::load(db);
}

fn load_hierarchies(db: &Db) {
    if let Some(net) = crate::routing::load(db) {
        crate::ch::preload(db, &net);
    }
}

fn load_changesets(db: &Db) {
    crate::cs::shared(db);
}

fn net_parts_bytes(db: &Db) -> u64 {
    // per mode a part per junction (4 bytes), only with the network in memory
    match crate::routing::loaded(db) {
        Some(net) => 3 * 4 * net.n() as u64,
        None => 0,
    }
}

fn net_parts(db: &Db) {
    if let Some(net) = crate::routing::loaded(db) {
        use knut_lang::ast::Mode as M;
        for m in [M::Foot, M::Bicycle, M::Car] {
            net.components(m);
        }
    }
}

fn boundary_bytes(db: &Db) -> u64 {
    // about 100 bytes per entry
    element_size(db, ElementKind::AdminBoundaries).map_or(0, |_| 4 << 20)
}

fn boundary_entries(db: &Db) {
    crate::boundaries::index(db);
}

fn notes_bytes(db: &Db) -> u64 {
    std::fs::metadata(crate::cs::sidecar(&db.path, "notes")).map_or(0, |m| 5 * m.len())
}

fn notes(db: &Db) {
    crate::notes::shared(db);
}

fn focus_bytes(_: &Db) -> u64 {
    0
}

fn focus(db: &Db) {
    db.focus();
}

fn user_names(db: &Db) {
    db.user_name(0);
}

/// The parts in the order they are loaded ahead: the most useful for their memory first.
fn plan(mode: Mode) -> Vec<Part> {
    use ElementKind::*;
    let mut v = vec![
        // a
        Part::Mapped(BlockTable),
        Part::Mapped(SpatialIndex),
        Part::Mapped(TagFilters),
    ];
    if mode == Mode::Minimal {
        return v;
    }
    v.extend([
        // b, small and often needed
        Part::Mapped(PlaceIndex),
        Part::Mapped(TagStatistics),
        Part::Mapped(AdminBoundaries),
        Part::Derived("boundary entries", boundary_bytes, boundary_entries),
        Part::Derived("map view", focus_bytes, focus),
        Part::Decoded(Users, 3.0, user_names),
        Part::Mapped(IdIndex),
        Part::Mapped(AddressIndex),
        Part::Mapped(NameIndex),
        // b, expensive to load on first use: decoded copies
        Part::Decoded(RoutingNetwork, 1.1, load_net),
        Part::Derived("connected parts of the network", net_parts_bytes, net_parts),
        Part::Decoded(Changesets, 5.0, load_changesets),
        Part::Derived("notes", notes_bytes, notes),
        Part::Decoded(RoutingCh, 1.1, load_hierarchies),
        Part::Decoded(RoutingCch, 1.1, load_hierarchies),
    ]);
    if mode == Mode::PreloadAll {
        v.extend([Part::Mapped(Relations), Part::Mapped(LargeObjects), Part::Mapped(Ways), Part::Mapped(Nodes)]);
    }
    v
}

/// Loads ahead what `mode` asks for, within the memory budget; `step` gets each part.
pub fn load(db: &Db, mode: Mode, mut step: impl FnMut(&Step)) {
    let budget = physical_memory().map_or(u64::MAX, |m| (m as f64 * mode.share()) as u64);
    let mut used: u64 = 0;
    let mut hierarchies_done = false;
    for part in plan(mode) {
        let (name, category, bytes, run): (String, Category, u64, Box<dyn Fn() + '_>) = match part {
            Part::Mapped(k) => {
                let Some(size) = element_size(db, k) else { continue };
                let el = db.reader.element(k).expect("element present");
                let run = move || {
                    for s in &el.sections {
                        if let Ok(b) = db.reader.section_bytes(s) {
                            touch(b);
                        }
                    }
                };
                (k.info().name_in_text(), category(k), size, Box::new(run))
            }
            Part::Decoded(k, factor, f) => {
                let Some(size) = element_size(db, k) else { continue };
                if matches!(k, ElementKind::RoutingCh | ElementKind::RoutingCch) {
                    // both kinds are loaded together
                    if hierarchies_done {
                        continue;
                    }
                    hierarchies_done = true;
                }
                (k.info().name_in_text(), Category::OnDemand, (size as f64 * factor) as u64, Box::new(move || f(db)))
            }
            Part::Derived(name, size, f) => {
                let b = size(db);
                if matches!(name, "connected parts of the network" | "notes") && b == 0 {
                    continue;
                }
                // (the notes are a side file, not derived)
                let c = if name == "notes" { Category::OnDemand } else { Category::Derived };
                (name.to_string(), c, b, Box::new(move || f(db)))
            }
        };
        // category a is loaded whatever the budget (it is small, and every query needs it)
        if category != Category::Always && used.saturating_add(bytes) > budget {
            step(&Step {
                name,
                category,
                bytes,
                seconds: 0.0,
                skipped: Some(format!("would exceed the memory budget ({:.1} of {:.1} GB); loaded on first use", (used + bytes) as f64 / 1e9, budget as f64 / 1e9)),
            });
            continue;
        }
        let t = Instant::now();
        run();
        used += bytes;
        step(&Step { name, category, bytes, seconds: t.elapsed().as_secs_f64(), skipped: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parts_of_the_database_have_their_categories() {
        assert_eq!(category(ElementKind::SpatialIndex), Category::Always);
        assert_eq!(category(ElementKind::TagFilters), Category::Always);
        assert_eq!(category(ElementKind::RoutingNetwork), Category::OnDemand);
        assert_eq!(category(ElementKind::Nodes), Category::OnDemand);
        assert!(physical_memory().is_some_and(|m| m > 1 << 28));
        // minimal loads only category a, and more is loaded the more the mode asks for
        let names = |m: Mode| plan(m).len();
        assert!(names(Mode::Minimal) < names(Mode::Preload) && names(Mode::Preload) < names(Mode::PreloadAll));
        assert!(plan(Mode::Minimal).iter().all(|p| matches!(p, Part::Mapped(k) if category(*k) == Category::Always)));
    }
}
