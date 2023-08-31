//! Notes (KNUT_LANG.md §7.2): imported once from a notes dump (`planet-notes-latest.osn`,
//! optionally bzip2-compressed) into `x.notes.knutidx`. Also reads changeset dumps.

use crate::cs::sidecar;
use crate::eval::Ctx;
use crate::set::{SetSrc, SetV};
use crate::value::*;
use knut_lang::diag::{Diagnostic, Span, Text2};
use quick_xml::Reader as XmlReader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone)]
struct RawNote {
    id: i64,
    lon: f64,
    lat: f64,
    created: i64,
    closed: Option<i64>,
    text: String,
    comments: u32,
}

pub struct NotesIndex {
    pub notes: Vec<Arc<NoteRec>>,
}

fn open(path: &Path) -> Result<Box<dyn BufRead>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let r = std::io::BufReader::with_capacity(1 << 20, f);
    if path.extension().map_or(false, |e| e == "bz2") {
        Ok(Box::new(std::io::BufReader::new(bzip2::read::MultiBzDecoder::new(r))))
    } else {
        Ok(Box::new(r))
    }
}

fn ts(s: &str) -> Option<i64> {
    knut_format::time::parse_time(s.trim_end_matches(" UTC").replace(' ', "T").as_str())
}

fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == name.as_bytes()).and_then(|a| a.unescape_value().ok()).map(|v| v.into_owned())
}

/// Parses a notes dump.
fn read_notes(path: &Path) -> Result<Vec<RawNote>, String> {
    let mut xr = XmlReader::from_reader(open(path)?);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<RawNote> = None;
    let mut in_comment = false;
    let mut text = String::new();
    loop {
        match xr.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"note" => {
                    cur = Some(RawNote {
                        id: attr(&e, "id").and_then(|v| v.parse().ok()).unwrap_or(0),
                        lon: attr(&e, "lon").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                        lat: attr(&e, "lat").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                        created: attr(&e, "created_at").and_then(|v| ts(&v)).unwrap_or(0),
                        closed: attr(&e, "closed_at").and_then(|v| ts(&v)),
                        text: String::new(),
                        comments: 0,
                    });
                }
                b"comment" => {
                    in_comment = true;
                    text.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_comment => {
                if let Ok(t) = t.unescape() {
                    text.push_str(&t);
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"comment" => {
                    in_comment = false;
                    if let Some(n) = &mut cur {
                        if n.comments == 0 {
                            n.text = text.trim().to_string();
                        }
                        n.comments += 1;
                    }
                }
                b"note" => {
                    if let Some(n) = cur.take() {
                        out.push(n);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("{}: {e}", path.display())),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Reads a changeset dump and calls `f(id, tags)` for every changeset.
pub fn read_changeset_dump(path: &Path, f: &mut dyn FnMut(u64, Vec<(String, String)>)) -> Result<(), String> {
    let mut xr = XmlReader::from_reader(open(path)?);
    let mut buf = Vec::new();
    let mut cur: Option<(u64, Vec<(String, String)>)> = None;
    loop {
        match xr.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == b"changeset" {
                    cur = Some((attr(&e, "id").and_then(|v| v.parse().ok()).unwrap_or(0), Vec::new()));
                }
            }
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"tag" => {
                    if let (Some(c), Some(k), Some(v)) = (&mut cur, attr(&e, "k"), attr(&e, "v")) {
                        c.1.push((k, v));
                    }
                }
                b"changeset" => {
                    let id = attr(&e, "id").and_then(|v| v.parse().ok()).unwrap_or(0);
                    f(id, Vec::new());
                }
                _ => {}
            },
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"changeset" {
                    if let Some((id, tags)) = cur.take() {
                        f(id, tags);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("{}: {e}", path.display())),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// `knut index notes DB dump.osn.bz2`.
pub fn build_sidecar(db_path: &Path, dump: &Path) -> Result<(PathBuf, usize), String> {
    let notes = read_notes(dump)?;
    let n = notes.len();
    let bytes = postcard::to_allocvec(&notes).map_err(|e| e.to_string())?;
    let z = zstd::encode_all(&bytes[..], 3).map_err(|e| e.to_string())?;
    let mut out = b"KNUTNT1\0".to_vec();
    out.extend_from_slice(&z);
    let path = sidecar(db_path, "notes");
    std::fs::write(&path, out).map_err(|e| e.to_string())?;
    Ok((path, n))
}

pub fn exists(db_path: &Path) -> bool {
    sidecar(db_path, "notes").exists()
}

pub fn index(ctx: &Ctx, span: Span) -> R<Arc<NotesIndex>> {
    ctx.caches
        .notes
        .get_or_init(|| {
            let db = ctx.db()?;
            let path = sidecar(&db.path, "notes");
            let err = |msg: String| {
                RtErr::fatal(
                    Diagnostic::error(
                        "E206",
                        span,
                        Text2::new("This needs the notes index, which does not exist yet.", "Dafür braucht es den Notes-Index, den es noch nicht gibt."),
                    )
                    .why(msg.clone(), msg)
                    .sugg(format!("knut index notes {} planet-notes-latest.osn.bz2", db.path.display())),
                )
            };
            let b = std::fs::read(&path).map_err(|e| err(format!("{}: {e}", path.display())))?;
            let z = b.strip_prefix(b"KNUTNT1\0").ok_or_else(|| err("not a notes index".into()))?;
            let raw = zstd::decode_all(z).map_err(|e| err(e.to_string()))?;
            let notes: Vec<RawNote> = postcard::from_bytes(&raw).map_err(|e| err(e.to_string()))?;
            let mut notes: Vec<Arc<NoteRec>> = notes
                .into_iter()
                .map(|n| {
                    Arc::new(NoteRec {
                        id: n.id,
                        lon: n.lon,
                        lat: n.lat,
                        created: n.created,
                        closed: n.closed,
                        status: Arc::from(if n.closed.is_some() { "closed" } else { "open" }),
                        text: Arc::from(n.text.as_str()),
                        comments: n.comments,
                    })
                })
                .collect();
            notes.sort_by_key(|n| n.id);
            Ok(Arc::new(NotesIndex { notes }))
        })
        .clone()
}

pub fn note_value(n: &Arc<NoteRec>) -> Value {
    let mut v = Ver::empty();
    v.ts = n.created;
    let mut e = Elem::new(EKind::Note, None, n.id, v, n.created);
    e.extra = Extra::Note(n.clone());
    let _ = e.geom.set(Arc::new(crate::geom::Geom::Point((n.lon, n.lat))));
    Value::Elem(Arc::new(e))
}

pub fn all_notes(ctx: &Ctx, span: Span) -> R<Value> {
    let ix = index(ctx, span)?;
    let n = ix.notes.len();
    let get = Arc::new(move |i: usize| note_value(&ix.notes[i]));
    Ok(Value::Set(Arc::new(SetV::lazy(SetSrc::Indexed { len: n, get }))))
}
