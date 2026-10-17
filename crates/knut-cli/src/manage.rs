//! `knut info` and `knut manage`: the contents of a container, adding and removing indexes,
//! compacting; interactively in the terminal or as commands.

use knut::container;
use knut_exec::indexes::{self, Progress};
use knut_format::container::{ElementKind, KINDS, Temporal};
use knut_format::layout::Header;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

// ----- formatting -------------------------------------------------------------------------

pub fn size(b: u64) -> String {
    let b = b as f64;
    if b < 1e3 {
        format!("{b:.0} B")
    } else if b < 1e6 {
        format!("{:.0} kB", b / 1e3)
    } else if b < 1e9 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.2} GB", b / 1e9)
    }
}

pub fn count(n: u64) -> String {
    let n = n as f64;
    if n < 1e4 {
        format!("{n:.0}")
    } else if n < 1e6 {
        format!("{:.1} k", n / 1e3)
    } else if n < 1e9 {
        format!("{:.1} M", n / 1e6)
    } else {
        format!("{:.2} G", n / 1e9)
    }
}

pub fn date(t: i64) -> String {
    knut_format::time::format_time(t).chars().take(10).collect()
}

fn bar(f: f64, width: usize) -> String {
    let n = (f * width as f64).round() as usize;
    format!("{}{}", "█".repeat(n.min(width)), "─".repeat(width - n.min(width)))
}

// ----- rows of the overview ----------------------------------------------------------------

#[derive(Clone)]
pub struct Row {
    pub id: Option<u32>,
    pub base: bool,
    pub kind: ElementKind,
    pub temporal: Temporal,
    pub size: Option<u64>,
    pub created: Option<i64>,
}

impl Row {
    pub fn built(&self) -> bool {
        self.id.is_some()
    }
}

/// All elements in the order of their kinds; with `missing`, also a row for every kind of
/// index that is not built.
pub fn rows(h: &Header, missing: bool) -> Vec<Row> {
    let mut out = Vec::new();
    for k in KINDS {
        let mut es: Vec<_> = h.directory.elements.iter().filter(|e| e.kind == k.kind).collect();
        es.sort_by_key(|e| (e.temporal as u8, e.id));
        for e in &es {
            out.push(Row { id: Some(e.id), base: k.base, kind: k.kind, temporal: e.temporal, size: Some(e.size()), created: Some(e.created) });
        }
        if es.is_empty() && missing && !k.base {
            out.push(Row { id: None, base: false, kind: k.kind, temporal: Temporal::None, size: None, created: None });
        }
    }
    out
}

const W_ID: usize = 4;
const W_NAME: usize = 37;
const W_SORT: usize = 21;
const W_TIME: usize = 8;
const W_BAR: usize = 10;

fn row_text(r: &Row, progress: Option<f64>) -> String {
    let info = r.kind.info();
    let id = match r.id {
        Some(i) if r.base => format!("{i}*"),
        Some(i) => format!("{i}"),
        None => "–".into(),
    };
    let status = match (progress, r.built()) {
        (Some(p), _) => format!("{} {:>3.0} %", bar(p, W_BAR), p * 100.0),
        (None, true) => format!("{} 100 %", bar(1.0, W_BAR)),
        (None, false) => "not built".to_string(),
    };
    format!(
        "{:>W_ID$}  {:<W_NAME$} {:<W_SORT$} {:<W_TIME$} {:<17} {:>9}  {}",
        id,
        info.name,
        info.sort,
        r.temporal.name(),
        status,
        r.size.map(size).unwrap_or_default(),
        r.created.map(date).unwrap_or_default()
    )
}

/// A row whose index waits for the running task.
fn row_text_waiting(r: &Row) -> String {
    let info = r.kind.info();
    format!("{:>W_ID$}  {:<W_NAME$} {:<W_SORT$} {:<W_TIME$} {:<17}", "–", info.name, info.sort, r.temporal.name(), "waiting...")
}

fn header_text() -> String {
    format!(
        "{:>W_ID$}  {:<W_NAME$} {:<W_SORT$} {:<W_TIME$} {:<17} {:>9}  {}",
        "ID", "Element", "Kind", "Time", "Status", "Size", "Created"
    )
}

/// Prints the table of elements (`knut info`, `knut manage … ls`).
pub fn print_table(h: &Header, missing: bool, explain: bool) {
    let tty = std::io::stdout().is_terminal();
    let (b, n, dim) = if tty { ("\x1b[1m", "\x1b[0m", "\x1b[2m") } else { ("", "", "") };
    println!("{dim}{}{n}", header_text());
    let mut base_done = false;
    for r in rows(h, missing) {
        if !r.base && !base_done {
            base_done = true;
            println!();
        }
        let style = if r.built() { b } else { dim };
        println!("{style}{}{n}", row_text(&r, None));
        if explain {
            println!("{dim}      {}{n}", r.kind.info().about);
        }
    }
    if rows(h, false).iter().any(|r| r.base) {
        println!("{dim}  * part of the database, cannot be removed{n}");
    }
}

/// The lines above the table of `knut info`.
pub fn print_summary(path: &Path, h: &Header, meta: Option<&str>) {
    let get = |k: &str| meta.and_then(|m| m.lines().find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string)));
    let v = h.version;
    let generator = get("generator").unwrap_or_default();
    println!("Format      {}.{}.{}{}", v[0], v[1], v[2], if generator.is_empty() { String::new() } else { format!(" · {generator}") });
    if let Some(s) = get("source") {
        println!("Source      {s}");
    }
    if let Some(l) = get("license") {
        println!("License     {l}");
    }
    println!();
    println!("Time        {} – {}", date(h.tmin), date(h.tmax));
    if let Ok(db) = knut_exec::db::Db::open(path) {
        let f = db.focus();
        let ew = |x: f64| if x < 0.0 { format!("{:.2}° W", -x) } else { format!("{x:.2}° E") };
        let ns = |y: f64| if y < 0.0 { format!("{:.2}° S", -y) } else { format!("{y:.2}° N") };
        println!("Area        {} – {}, {} – {}", ew(f.minx), ew(f.maxx), ns(f.miny), ns(f.maxy));
    }
    let o = h.n_objects;
    let ve = h.n_versions;
    println!("Objects     {} nodes · {} ways · {} relations", count(o[0]), count(o[1]), count(o[2]));
    println!("Versions    {} nodes · {} ways · {} relations", count(ve[0]), count(ve[1]), count(ve[2]));
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let free = container::freeable(path, h).unwrap_or(0);
    println!("Size        {}{}", size(len), if free > 0 { format!(" · {} can be freed (knut manage FILE compact)", size(free)) } else { String::new() });
}

// ----- commands ---------------------------------------------------------------------------

pub enum Command {
    Ls { explain: bool },
    Add { what: Vec<String>, temporal: Option<String> },
    Rm { id: u32, yes: bool },
    Compact,
    Upgrade,
}

fn kind_of(what: &str) -> Result<ElementKind, String> {
    KINDS
        .iter()
        .find(|k| k.addable && (k.command == what || k.name.eq_ignore_ascii_case(what)))
        .map(|k| k.kind)
        .ok_or_else(|| {
            let names: Vec<&str> = KINDS.iter().filter(|k| k.addable).map(|k| k.command).collect();
            format!("unknown kind of index \"{what}\"; one of: {}", names.join(", "))
        })
}

fn progress_line(label: &str, f: f64) {
    eprint!("\r{label}  {} {:>3.0} %", bar(f, 30), f * 100.0);
}

pub fn run(path: &Path, cmd: Option<Command>) -> Result<(), String> {
    if container::is_legacy(path) && !matches!(cmd, Some(Command::Upgrade)) {
        return Err(format!("{} has format 1.0; convert it first:  knut manage {} upgrade", path.display(), path.display()));
    }
    let Some(cmd) = cmd else {
        if !std::io::stdout().is_terminal() {
            return run(path, Some(Command::Ls { explain: false }));
        }
        return tui::run(path.to_path_buf());
    };
    match cmd {
        Command::Ls { explain } => {
            let h = container::header(path).map_err(|e| e.to_string())?;
            let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let free = container::freeable(path, &h).unwrap_or(0);
            let v = h.version;
            println!(
                "{} · format {}.{}.{} · {}{}\n",
                path.display(),
                v[0],
                v[1],
                v[2],
                size(len),
                if free > 0 { format!(" · {} can be freed (compact)", size(free)) } else { String::new() }
            );
            print_table(&h, true, explain);
            Ok(())
        }
        Command::Add { what, temporal } => {
            // all checked before the first is built; then one after the other, the others waiting
            let mut jobs = Vec::new();
            let h = container::header(path).map_err(|e| e.to_string())?;
            for w in &what {
                let kind = kind_of(w)?;
                let t = match &temporal {
                    Some(t) => Temporal::parse(t).ok_or_else(|| format!("unknown time \"{t}\"; one of: latest, daily, weekly, monthly, yearly, full"))?,
                    None => indexes::supported(kind).first().copied().unwrap_or(Temporal::Latest),
                };
                if let Some(id) = indexes::existing(&h, kind, t) {
                    return Err(format!("the {} ({}) is already built as element {id}; to build it again, remove it first: knut manage FILE rm {id}", kind.info().name_in_text(), t.name()));
                }
                if !jobs.contains(&(kind, t)) {
                    jobs.push((kind, t));
                }
            }
            for (i, &(kind, t)) in jobs.iter().enumerate() {
                let waiting: Vec<&str> = jobs[i + 1..].iter().map(|(k, _)| k.info().name).collect();
                if !waiting.is_empty() {
                    eprintln!("waiting: {}", waiting.join(", "));
                }
                let p = Progress::default();
                let label = kind.info().name;
                let done = std::sync::atomic::AtomicBool::new(false);
                let id = std::thread::scope(|s| {
                    s.spawn(|| {
                        while !done.load(std::sync::atomic::Ordering::Relaxed) {
                            progress_line(label, p.get());
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    });
                    let r = indexes::add(path, kind, t, &p);
                    done.store(true, std::sync::atomic::Ordering::Relaxed);
                    r
                });
                eprintln!();
                let id = id?;
                println!("added element {id}: {label} ({})", t.name());
            }
            Ok(())
        }
        Command::Rm { id, yes } => {
            let h = container::header(path).map_err(|e| e.to_string())?;
            let e = h.directory.get(id).ok_or_else(|| format!("there is no element {id}"))?;
            if e.info().base {
                return Err(format!("element {id} ({}) is part of the database and cannot be removed", e.info().name));
            }
            if let Some(by) = indexes::needed_by(&h, e.kind) {
                return Err(format!("element {id} ({}) is needed by {by}; remove {} first", e.info().name, if by.contains(" and ") { "them" } else { "it" }));
            }
            if !yes {
                eprint!("remove element {id} ({}, {})? [y/N] ", e.info().name, size(e.size()));
                let mut a = String::new();
                std::io::stdin().read_line(&mut a).map_err(|e| e.to_string())?;
                if !a.trim().eq_ignore_ascii_case("y") {
                    return Ok(());
                }
            }
            container::remove_element(path, id).map_err(|e| e.to_string())?;
            let h = container::header(path).map_err(|e| e.to_string())?;
            let free = container::freeable(path, &h).unwrap_or(0);
            println!("removed element {id}; {} can be freed with: knut manage {} compact", size(free), path.display());
            Ok(())
        }
        Command::Compact => {
            let freed = container::compact(path, &mut |d, t| progress_line("Compacting", d as f64 / t.max(1) as f64)).map_err(|e| e.to_string())?;
            eprintln!();
            println!("freed {}", size(freed));
            Ok(())
        }
        Command::Upgrade => {
            if !container::is_legacy(path) {
                println!("{} already has format 0.1", path.display());
                return Ok(());
            }
            container::upgrade(path, &mut |d, t| progress_line("Converting", d as f64 / t.max(1) as f64)).map_err(|e| e.to_string())?;
            eprintln!();
            println!("converted {} to format 0.1.0", path.display());
            Ok(())
        }
    }
}

// ----- interactive ------------------------------------------------------------------------

mod tui {
    use super::*;
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Modifier, Style, Stylize};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
    use std::time::Duration;

    enum Mode {
        Normal,
        ConfirmRemove(u32),
        /// choosing the time of a new index: kind, options, selected
        Time(ElementKind, Vec<Temporal>, usize),
    }

    struct Job {
        label: String,
        /// the row it builds (kind), or compacting
        kind: Option<ElementKind>,
        progress: Progress,
        handle: std::thread::JoinHandle<Result<String, String>>,
    }

    struct App {
        path: PathBuf,
        rows: Vec<Row>,
        header: Header,
        free: u64,
        len: u64,
        state: ListState,
        mode: Mode,
        message: String,
        job: Option<Job>,
        /// indexes chosen while another task runs: built in turn
        waiting: std::collections::VecDeque<(ElementKind, Temporal)>,
    }

    impl App {
        fn reload(&mut self) -> Result<(), String> {
            self.header = container::header(&self.path).map_err(|e| e.to_string())?;
            self.rows = rows(&self.header, true);
            self.free = container::freeable(&self.path, &self.header).unwrap_or(0);
            self.len = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
            let n = self.rows.len();
            if self.state.selected().is_none_or(|i| i >= n) {
                self.state.select(Some(n.saturating_sub(1).min(self.state.selected().unwrap_or(0))));
            }
            Ok(())
        }

        fn selected(&self) -> Option<&Row> {
            self.state.selected().and_then(|i| self.rows.get(i))
        }
    }

    pub fn run(path: PathBuf) -> Result<(), String> {
        let header = container::header(&path).map_err(|e| e.to_string())?;
        let mut app = App {
            path,
            rows: Vec::new(),
            header,
            free: 0,
            len: 0,
            state: ListState::default(),
            mode: Mode::Normal,
            message: String::new(),
            job: None,
            waiting: std::collections::VecDeque::new(),
        };
        app.reload()?;
        // start on the first index
        app.state.select(app.rows.iter().position(|r| !r.base).or(Some(0)));
        let mut term = ratatui::init();
        let r = event_loop(&mut term, &mut app);
        ratatui::restore();
        r
    }

    fn event_loop(term: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<(), String> {
        loop {
            if app.job.as_ref().is_some_and(|j| j.handle.is_finished()) {
                let j = app.job.take().unwrap();
                app.message = match j.handle.join() {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => format!("{}: {e}", j.label),
                    Err(_) => format!("{} failed", j.label),
                };
                app.reload()?;
                // the next waiting index (unless it was built meanwhile)
                while app.job.is_none() {
                    let Some((kind, t)) = app.waiting.pop_front() else { break };
                    if indexes::existing(&app.header, kind, t).is_none() {
                        start_add(app, kind, t);
                    }
                }
            }
            term.draw(|f| draw(f, app)).map_err(|e| e.to_string())?;
            if !event::poll(Duration::from_millis(150)).map_err(|e| e.to_string())? {
                continue;
            }
            let Event::Key(k) = event::read().map_err(|e| e.to_string())? else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match std::mem::replace(&mut app.mode, Mode::Normal) {
                Mode::ConfirmRemove(id) => {
                    if matches!(k.code, KeyCode::Char('y')) {
                        app.message = match container::remove_element(&app.path, id) {
                            Ok(e) => format!("removed element {id} ({})", e.info().name),
                            Err(e) => e.to_string(),
                        };
                        app.reload()?;
                    }
                }
                Mode::Time(kind, opts, i) => match k.code {
                    KeyCode::Up => app.mode = Mode::Time(kind, opts, i.saturating_sub(1)),
                    KeyCode::Down => {
                        let n = opts.len();
                        app.mode = Mode::Time(kind, opts, (i + 1).min(n - 1));
                    }
                    KeyCode::Enter => match indexes::existing(&app.header, kind, opts[i]) {
                        Some(id) => app.message = format!("the {} ({}) is already built (element {id}); ⌫ removes it", kind.info().name_in_text(), opts[i].name()),
                        None => add_or_wait(app, kind, opts[i]),
                    },
                    _ => {}
                },
                Mode::Normal => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if app.job.is_some() {
                            app.message = if app.waiting.is_empty() { "wait until the running task has finished".into() } else { "wait until the running and the waiting tasks have finished".into() };
                        } else {
                            return Ok(());
                        }
                    }
                    // the selection stays on the rows (ListState itself does not know their number)
                    KeyCode::Up => app.state.select(Some(app.state.selected().unwrap_or(0).saturating_sub(1))),
                    KeyCode::Down => {
                        let last = app.rows.len().saturating_sub(1);
                        app.state.select(Some((app.state.selected().unwrap_or(0) + 1).min(last)));
                    }
                    KeyCode::Enter | KeyCode::Char('t') => {
                        let Some(r) = app.selected().cloned() else { continue };
                        if r.base {
                            app.message = format!("{} is part of the database", r.kind.info().name);
                        } else {
                            let opts = indexes::supported(r.kind).to_vec();
                            if opts.is_empty() {
                                app.message = format!("the {} cannot be built yet", r.kind.info().name_in_text());
                            } else if opts.len() == 1 || k.code != KeyCode::Char('t') {
                                match indexes::existing(&app.header, r.kind, opts[0]) {
                                    Some(id) => app.message = format!("the {} is already built (element {id}); ⌫ removes it", r.kind.info().name_in_text()),
                                    None => add_or_wait(app, r.kind, opts[0]),
                                }
                            } else {
                                app.mode = Mode::Time(r.kind, opts, 0);
                            }
                        }
                    }
                    KeyCode::Backspace | KeyCode::Delete => match app.selected() {
                        Some(r) if r.base => app.message = format!("{} is part of the database and cannot be removed", r.kind.info().name),
                        Some(r @ Row { id: Some(id), .. }) => match indexes::needed_by(&app.header, r.kind) {
                            Some(by) => app.message = format!("the {} is needed by {by}; remove {} first", r.kind.info().name_in_text(), if by.contains(" and ") { "them" } else { "it" }),
                            None => app.mode = Mode::ConfirmRemove(*id),
                        },
                        _ => {}
                    },
                    KeyCode::Char('c') => {
                        if app.job.is_some() {
                            app.message = "another task is running".into();
                        } else if app.free == 0 {
                            app.message = "nothing to free".into();
                        } else {
                            start_compact(app);
                        }
                    }
                    _ => {}
                },
            }
        }
    }

    /// Builds the index now, or after the running task (and those waiting before it).
    fn add_or_wait(app: &mut App, kind: ElementKind, t: Temporal) {
        let running = app.job.as_ref().is_some_and(|j| j.kind == Some(kind));
        if running || app.waiting.contains(&(kind, t)) {
            app.message = format!("the {} is already on its way", kind.info().name_in_text());
        } else if app.job.is_some() {
            app.waiting.push_back((kind, t));
            app.message = format!("the {} waits until the running task has finished", kind.info().name_in_text());
        } else {
            start_add(app, kind, t);
        }
    }

    fn start_add(app: &mut App, kind: ElementKind, t: Temporal) {
        let p = Progress::default();
        let (path, p2) = (app.path.clone(), p.clone());
        let handle = std::thread::spawn(move || {
            indexes::add(&path, kind, t, &p2).map(|id| format!("added element {id}: {} ({})", kind.info().name, t.name()))
        });
        app.message = format!("building the {} …", kind.info().name_in_text());
        app.job = Some(Job { label: kind.info().name.into(), kind: Some(kind), progress: p, handle });
    }

    fn start_compact(app: &mut App) {
        let p = Progress::default();
        let (path, p2) = (app.path.clone(), p.clone());
        let handle = std::thread::spawn(move || {
            container::compact(&path, &mut |d, t| p2.set(d as f64 / t.max(1) as f64)).map(|f| format!("freed {}", size(f))).map_err(|e| e.to_string())
        });
        app.message = "compacting …".into();
        app.job = Some(Job { label: "Compacting".into(), kind: None, progress: p, handle });
    }

    fn draw(f: &mut ratatui::Frame, app: &mut App) {
        let [top, list, about, bottom] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(6), Constraint::Length(2)]).areas(f.area());
        let v = app.header.version;
        let mut head = vec![
            Span::raw(format!("{}", app.path.display())).bold(),
            Span::raw(format!("   format {}.{}.{} · {}", v[0], v[1], v[2], size(app.len))),
        ];
        if app.free > 0 {
            head.push(Span::raw(format!(" · {} can be freed (c: compact)", size(app.free))).yellow());
        }
        if let Some(j) = app.job.as_ref().filter(|j| j.kind.is_none()) {
            head.push(Span::raw(format!("   {} {} {:.0} %", j.label, bar(j.progress.get(), 20), j.progress.get() * 100.0)).cyan());
        }
        // the rows start after the room of the selection mark ("▸ ")
        f.render_widget(Paragraph::new(vec![Line::from(head), Line::from(Span::raw(format!("  {}", header_text())).dim())]), top);

        let job_kind = app.job.as_ref().and_then(|j| j.kind);
        let mut items = Vec::new();
        let mut index_of = Vec::new();
        let mut base_done = false;
        for (i, r) in app.rows.iter().enumerate() {
            if !r.base && !base_done {
                base_done = true;
                items.push(ListItem::new(Line::from(Span::raw("").dim())));
                index_of.push(None);
            }
            let progress = (job_kind == Some(r.kind) && !r.built()).then(|| app.job.as_ref().unwrap().progress.get());
            let waiting = !r.built() && progress.is_none() && app.waiting.iter().any(|(k, _)| *k == r.kind);
            let style = if r.built() { Style::default().add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::DIM) };
            let text = if waiting { row_text_waiting(r) } else { row_text(r, progress) };
            items.push(ListItem::new(Line::from(Span::styled(text, style))));
            index_of.push(Some(i));
        }
        // map the selected row to the list position (the separator line is not selectable)
        let mut ls = ListState::default();
        ls.select(app.state.selected().and_then(|s| index_of.iter().position(|x| *x == Some(s))));
        f.render_stateful_widget(
            List::new(items).highlight_style(Style::default().reversed()).highlight_symbol("▸ ").highlight_spacing(ratatui::widgets::HighlightSpacing::Always),
            list,
            &mut ls,
        );

        let text = match &app.mode {
            Mode::Time(kind, opts, i) => {
                let mut l = vec![Line::from(format!("Time of the {}:", kind.info().name_in_text()))];
                for (j, t) in opts.iter().enumerate() {
                    let s = format!("  {}", t.name());
                    l.push(if j == *i { Line::from(s).reversed() } else { Line::from(s) });
                }
                l
            }
            _ => match app.selected() {
                Some(r) => {
                    let info = r.kind.info();
                    let mut l = vec![Line::from(Span::raw(info.about))];
                    if !r.base && !r.built() {
                        l.push(Line::from(Span::raw(format!("Add: Enter (latest) or t (choose the time) · knut manage FILE add {}", info.command)).dim()));
                    }
                    l
                }
                None => vec![],
            },
        };
        let title = app.selected().map(|r| format!(" {} ", r.kind.info().name)).unwrap_or_default();
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(Block::default().borders(Borders::ALL).title(title)), about);

        let keys = match app.mode {
            Mode::ConfirmRemove(id) => format!("Remove element {id}? y: yes · any other key: no"),
            Mode::Time(..) => "↑↓ choose · Enter build · any other key: cancel".into(),
            Mode::Normal => "↑↓ select · Enter add · t add with time · ⌫ remove · c compact · q quit".into(),
        };
        f.render_widget(Paragraph::new(vec![Line::from(Span::raw(app.message.clone()).yellow()), Line::from(Span::raw(keys).dim())]), bottom);
    }
}
