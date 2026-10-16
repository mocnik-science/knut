//! Optional indexes of a container (`knut manage … add`): which kinds can be built, and
//! building them from the database.

use knut::container::NewSection;
use knut_format::container::{ElementKind, Temporal};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Progress of a build, in millionths (shared with the interface that shows it).
#[derive(Clone, Default)]
pub struct Progress(Arc<AtomicU64>);

impl Progress {
    pub fn set(&self, fraction: f64) {
        self.0.store((fraction.clamp(0.0, 1.0) * 1e6) as u64, Ordering::Relaxed);
    }

    pub fn get(&self) -> f64 {
        self.0.load(Ordering::Relaxed) as f64 / 1e6
    }
}

/// A built index: sections and parameters of the new element.
pub struct Built {
    pub sections: Vec<NewSection>,
    pub params: Vec<(String, String)>,
}

/// Kinds that can be built by this version, with the moments they support.
pub fn supported(kind: ElementKind) -> &'static [Temporal] {
    match kind {
        ElementKind::PlaceIndex | ElementKind::NameIndex | ElementKind::AddressIndex | ElementKind::TagStatistics => &[Temporal::Latest],
        ElementKind::Changesets => &[Temporal::Full],
        ElementKind::RoutingNetwork | ElementKind::RoutingCh | ElementKind::RoutingCch => &[Temporal::Latest],
        _ => &[],
    }
}

/// Runs `f` with a controller whose block counts drive `progress` from `from` to `to`.
fn with_progress<T: Send>(progress: &Progress, from: f64, to: f64, f: impl FnOnce(&crate::control::Control) -> T + Send) -> T {
    let ctl = crate::control::Control::new();
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                let p = ctl.progress();
                if p.blocks_total > 0 {
                    progress.set(from + (to - from) * p.blocks_done as f64 / p.blocks_total as f64);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let r = f(&ctl);
        done.store(true, Ordering::Relaxed);
        progress.set(to);
        r
    })
}

fn time_param(db: &crate::db::Db) -> (String, String) {
    ("at".into(), knut_format::time::format_time(db.latest))
}

/// Builds an index of `kind` for the database at `path`.
pub fn build(path: &Path, kind: ElementKind, temporal: Temporal, progress: &Progress) -> Result<Built, String> {
    if !supported(kind).contains(&temporal) {
        return Err(if supported(kind).is_empty() {
            format!("the {} cannot be built yet", kind.info().name_in_text())
        } else {
            format!("the {} can be built for: {}", kind.info().name_in_text(), supported(kind).iter().map(|t| t.name()).collect::<Vec<_>>().join(", "))
        });
    }
    let db = crate::db::Db::open(path)?;
    let err = |e: crate::value::RtErr| format!("{}: {}", e.diag.code, e.diag.title.en);
    match kind {
        ElementKind::PlaceIndex => {
            let places = with_progress(progress, 0.0, 0.95, |c| crate::places::collect(&db, Some(c))).map_err(err)?;
            let bytes = crate::places::encode(&places);
            Ok(Built {
                sections: vec![NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 0, bytes }],
                params: vec![time_param(&db), ("places".into(), places.len().to_string())],
            })
        }
        ElementKind::TagStatistics => {
            let keys = with_progress(progress, 0.0, 0.95, |c| crate::special::key_stats(&db, Some(c), db.latest)).map_err(err)?;
            let bytes = crate::special::encode_key_stats(db.latest, &keys);
            Ok(Built {
                sections: vec![NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 1, bytes }],
                params: vec![time_param(&db), ("keys".into(), keys.len().to_string())],
            })
        }
        ElementKind::NameIndex => {
            let names = with_progress(progress, 0.0, 0.8, |c| crate::geocode::collect_names(&db, Some(c))).map_err(err)?;
            let bytes = crate::geocode::encode_names(&names);
            Ok(Built {
                sections: vec![NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 0, bytes }],
                params: vec![time_param(&db), ("names".into(), names.len().to_string())],
            })
        }
        ElementKind::AddressIndex => {
            let addrs = with_progress(progress, 0.0, 0.8, |c| crate::geocode::collect_addresses(&db, Some(c))).map_err(err)?;
            let bytes = crate::geocode::encode_addresses(&addrs);
            Ok(Built {
                sections: vec![NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 0, bytes }],
                params: vec![time_param(&db), ("addresses".into(), addrs.len().to_string())],
            })
        }
        ElementKind::RoutingNetwork => {
            let net = with_progress(progress, 0.0, 0.9, |c| crate::routing::build_network(&db, Some(c))).map_err(err)?;
            let bytes = crate::routing::encode(&net);
            Ok(Built {
                sections: vec![NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 0, bytes }],
                params: vec![time_param(&db), ("junctions".into(), net.n().to_string()), ("edges".into(), (net.to.len() / 2).to_string())],
            })
        }
        ElementKind::RoutingCh | ElementKind::RoutingCch => {
            let r = knut::Reader::open(path).map_err(|e| e.to_string())?;
            let bytes = element_bytes(&r, ElementKind::RoutingNetwork)?;
            let net = crate::routing::decode(bytes)?;
            let network = network_checksum(&r);
            drop(r);
            let mut sections = Vec::new();
            let topo = (kind == ElementKind::RoutingCch).then(|| {
                let rank = crate::ch::cch_order(&net);
                progress.set(0.1);
                crate::ch::cch_topology(&net, rank, &|f| progress.set(0.1 + 0.4 * f))
            });
            // the modes in parallel; progress: the mean of the three
            let done: Vec<std::sync::atomic::AtomicU64> = (0..3).map(|_| Default::default()).collect();
            let base = if topo.is_some() { 0.5 } else { 0.0 };
            let report = |i: usize, f: f64| {
                done[i].store((f * 1e6) as u64, Ordering::Relaxed);
                let mean = done.iter().map(|x| x.load(Ordering::Relaxed) as f64 / 1e6).sum::<f64>() / 3.0;
                progress.set(base + (0.95 - base) * mean);
            };
            // CCH: the topology once, then all modes customized at once; CH: one mode after the
            // other (each contraction is parallel itself and needs much memory). Each result is
            // encoded at once, so that only its bytes stay in memory.
            let index = knut_format::layout::SectionKind::Index as u32;
            match &topo {
                Some(t) => {
                    sections.push(NewSection { kind: index, flags: 0, bytes: crate::ch::encode_topology(t) });
                    for (i, (m, w)) in crate::ch::MODES.iter().zip(t.customize_all(&net)).enumerate() {
                        sections.push(NewSection { kind: index + 1 + i as u32, flags: 0, bytes: crate::ch::encode_weights(*m, &w) });
                        report(i, 1.0);
                    }
                }
                None => {
                    for (i, m) in crate::ch::MODES.iter().enumerate() {
                        let h = crate::ch::build_ch(&net, *m, &|f| report(i, f));
                        sections.push(NewSection { kind: index + i as u32, flags: 0, bytes: crate::ch::encode(*m, &h) });
                        report(i, 1.0);
                    }
                }
            }
            Ok(Built { sections, params: vec![time_param(&db), ("modes".into(), "foot, bicycle, car".into()), ("network".into(), network)] })
        }
        ElementKind::Changesets => {
            progress.set(0.05);
            let bytes = crate::cs::build_section(&db, None, &mut |_| {})?;
            Ok(Built { sections: vec![NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 1, bytes }], params: vec![time_param(&db)] })
        }
        _ => unreachable!(),
    }
}

/// Builds the index and adds it to the container; returns the new element id.
pub fn add(path: &Path, kind: ElementKind, temporal: Temporal, progress: &Progress) -> Result<u32, String> {
    let h = knut::container::header(path).map_err(|e| e.to_string())?;
    if let Some(id) = existing(&h, kind, temporal) {
        return Err(format!(
            "the {} ({}) is already built as element {id}; to build it again, remove it first: knut manage FILE rm {id}",
            kind.info().name_in_text(),
            temporal.name()
        ));
    }
    let b = build(path, kind, temporal, progress)?;
    let id = knut::container::add_element(path, kind, temporal, b.params, b.sections).map_err(|e| e.to_string())?;
    progress.set(1.0);
    Ok(id)
}

/// The checksum of the routing network, as stored with the hierarchies built on it (they are
/// only used while it matches).
pub fn network_checksum(r: &knut::Reader) -> String {
    format!("{:016x}", r.element(ElementKind::RoutingNetwork).and_then(|e| e.sections.first()).map_or(0, |s| s.checksum))
}

/// The elements that need an element of `kind` (the hierarchies need the routing network),
/// as "element 27 (Contraction hierarchies)", or None.
pub fn needed_by(h: &knut_format::layout::Header, kind: ElementKind) -> Option<String> {
    if kind != ElementKind::RoutingNetwork {
        return None;
    }
    let list: Vec<String> = h
        .directory
        .elements
        .iter()
        .filter(|e| matches!(e.kind, ElementKind::RoutingCh | ElementKind::RoutingCch))
        .map(|e| format!("element {} ({})", e.id, e.kind.info().name))
        .collect();
    (!list.is_empty()).then(|| list.join(" and "))
}

/// The element of `kind` for the same time, if the container has one (nothing is built twice).
pub fn existing(h: &knut_format::layout::Header, kind: ElementKind, temporal: Temporal) -> Option<u32> {
    h.directory.elements.iter().find(|e| e.kind == kind && e.temporal == temporal).map(|e| e.id)
}

/// The bytes of the newest element of `kind`, or why there is none.
pub fn element_bytes<'a>(r: &'a knut::Reader, kind: ElementKind) -> Result<&'a [u8], String> {
    let el = r.element(kind).ok_or_else(|| format!("this database has no {}; add it with: knut manage FILE add {}", kind.info().name_in_text(), kind.info().command))?;
    let s = el.sections.first().ok_or("empty element")?;
    r.section_bytes(s).map_err(|e| e.to_string())
}
