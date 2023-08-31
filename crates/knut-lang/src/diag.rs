//! Diagnostics: errors and warnings with source location, explanation and suggestions,
//! rendered in English or German (KNUT_LANG.md §8, Nutzerbeschreibung Teil 3).

use std::fmt::Write;

/// Byte range in the script source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span { start: start as u32, end: end as u32 }
    }

    pub fn to(self, o: Span) -> Span {
        Span { start: self.start.min(o.start), end: self.end.max(o.end) }
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    En,
    De,
}

/// A text in both message languages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Text2 {
    pub en: String,
    pub de: String,
}

impl Text2 {
    pub fn new(en: impl Into<String>, de: impl Into<String>) -> Text2 {
        Text2 { en: en.into(), de: de.into() }
    }

    /// Same text in both languages (code, numbers).
    pub fn same(s: impl Into<String>) -> Text2 {
        let s = s.into();
        Text2 { en: s.clone(), de: s }
    }

    pub fn get(&self, l: Lang) -> &str {
        match l {
            Lang::En => &self.en,
            Lang::De => &self.de,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

/// A copyable suggestion: code plus optional note in parentheses.
#[derive(Clone, Debug, PartialEq)]
pub struct Sugg {
    /// Optional label in front of the code (`- keep them:`).
    pub label: Option<Text2>,
    pub code: String,
    pub note: Option<Text2>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub span: Span,
    /// Further positions marked on the same line (W005).
    pub extra: Vec<Span>,
    pub title: Text2,
    pub why: Vec<Text2>,
    pub header: Option<Text2>,
    pub suggestions: Vec<Sugg>,
    /// Lines after the suggestions.
    pub trailer: Vec<Text2>,
}

impl Diagnostic {
    pub fn error(code: &'static str, span: Span, title: Text2) -> Diagnostic {
        Diagnostic {
            code,
            severity: Severity::Error,
            span,
            extra: Vec::new(),
            title,
            why: Vec::new(),
            header: None,
            suggestions: Vec::new(),
            trailer: Vec::new(),
        }
    }

    pub fn warning(code: &'static str, span: Span, title: Text2) -> Diagnostic {
        Diagnostic { severity: Severity::Warning, ..Diagnostic::error(code, span, title) }
    }

    pub fn why(mut self, en: impl Into<String>, de: impl Into<String>) -> Diagnostic {
        self.why.push(Text2::new(en, de));
        self
    }

    pub fn header(mut self, h: Header) -> Diagnostic {
        self.header = Some(h.text());
        self
    }

    pub fn sugg(mut self, code: impl Into<String>) -> Diagnostic {
        self.suggestions.push(Sugg { label: None, code: code.into(), note: None });
        self
    }

    pub fn sugg_note(mut self, code: impl Into<String>, en: &str, de: &str) -> Diagnostic {
        self.suggestions.push(Sugg { label: None, code: code.into(), note: Some(Text2::new(en, de)) });
        self
    }

    pub fn sugg_label(mut self, en: &str, de: &str, code: impl Into<String>) -> Diagnostic {
        self.suggestions.push(Sugg { label: Some(Text2::new(en, de)), code: code.into(), note: None });
        self
    }

    pub fn trailer(mut self, en: impl Into<String>, de: impl Into<String>) -> Diagnostic {
        self.trailer.push(Text2::new(en, de));
        self
    }

    pub fn extra(mut self, s: Span) -> Diagnostic {
        self.extra.push(s);
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Standard suggestion headers.
#[derive(Clone, Copy)]
pub enum Header {
    DidYouMean,
    DidYouMeanOne,
    YouCan,
    PleaseWrite,
    ForExample,
    Consider,
}

impl Header {
    pub fn text(self) -> Text2 {
        match self {
            Header::DidYouMean => Text2::new("Did you mean:", "Meintest du:"),
            Header::DidYouMeanOne => Text2::new("Did you mean one of these?", "Meintest du eines davon?"),
            Header::YouCan => Text2::new("You can:", "Du kannst:"),
            Header::PleaseWrite => Text2::new("Please write it as one of these:", "Bitte schreibe es so:"),
            Header::ForExample => Text2::new("For example:", "Zum Beispiel:"),
            Header::Consider => Text2::new("Consider a clearer name, for example:", "Besser ist ein eindeutiger Name, etwa:"),
        }
    }
}

/// Line index (0-based) and column (in chars) of a byte offset.
pub fn line_col(src: &str, off: u32) -> (usize, usize) {
    let off = (off as usize).min(src.len());
    let before = &src[..off];
    let line = before.matches('\n').count();
    let ls = before.rfind('\n').map_or(0, |p| p + 1);
    (line, src[ls..off].chars().count())
}

fn line_text(src: &str, line: usize) -> &str {
    src.split('\n').nth(line).unwrap_or("").trim_end_matches('\r')
}

/// Renders one diagnostic in the house style (two spaces after the header colon).
pub fn render(d: &Diagnostic, src: &str, file: Option<&str>, lang: Lang) -> String {
    let mut s = String::new();
    let kind = match (d.severity, lang) {
        (Severity::Error, Lang::En) => "Error",
        (Severity::Error, Lang::De) => "Fehler",
        (Severity::Warning, Lang::En) => "Warning",
        (Severity::Warning, Lang::De) => "Warnung",
    };
    let _ = writeln!(s, "{kind} [{}]:  {}", d.code, d.title.get(lang));
    if !d.span.is_empty() || d.span.start > 0 || !src.is_empty() {
        let (line, col) = line_col(src, d.span.start);
        let text = line_text(src, line);
        if !text.is_empty() {
            let no = format!("{}", line + 1);
            let _ = writeln!(s);
            if let Some(f) = file {
                let _ = writeln!(s, "  --> {f}:{}:{}", line + 1, col + 1);
            }
            let _ = writeln!(s, "  {no} | {text}");
            // markers for the primary span and extra spans on the same line
            let mut marks: Vec<(usize, usize)> = Vec::new();
            for sp in std::iter::once(&d.span).chain(d.extra.iter()) {
                let (l, c) = line_col(src, sp.start);
                if l != line {
                    continue;
                }
                let (l2, c2) = line_col(src, sp.end.max(sp.start + 1));
                let len = if l2 == l { (c2 - c).max(1) } else { text.chars().count().saturating_sub(c).max(1) };
                marks.push((c, len));
            }
            marks.sort_unstable();
            let mut m = String::new();
            for (c, len) in marks {
                while m.chars().count() < c {
                    m.push(' ');
                }
                for _ in 0..len {
                    m.push('^');
                }
            }
            let pad = " ".repeat(no.len() + 5);
            let _ = writeln!(s, "{pad}{}", m.trim_end());
        }
    }
    for w in &d.why {
        for l in w.get(lang).lines() {
            let _ = writeln!(s, "  {l}");
        }
    }
    if !d.suggestions.is_empty() {
        if let Some(h) = &d.header {
            if !d.why.is_empty() {
                let _ = writeln!(s);
            }
            let _ = writeln!(s, "  {}", h.get(lang));
        }
        let width = d.suggestions.iter().filter(|g| g.note.is_some()).map(|g| g.code.chars().count()).max().unwrap_or(0);
        let lwidth = d
            .suggestions
            .iter()
            .filter_map(|g| g.label.as_ref())
            .map(|l| l.get(lang).chars().count())
            .max()
            .unwrap_or(0);
        for g in &d.suggestions {
            let mut line = String::from("    ");
            if let Some(l) = &g.label {
                let t = format!("- {}", l.get(lang));
                let _ = write!(line, "{t:<w$}   ", w = lwidth + 2);
            }
            match &g.note {
                Some(n) => {
                    let _ = write!(line, "{:<width$}     ({})", g.code, n.get(lang));
                }
                None => line.push_str(&g.code),
            }
            let _ = writeln!(s, "{}", line.trim_end());
        }
    }
    for t in &d.trailer {
        for l in t.get(lang).lines() {
            let _ = writeln!(s, "  {l}");
        }
    }
    s
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// Summary line, e.g. `1 error and 2 warnings found. Nothing was run.`
pub fn summary(errors: usize, warnings: usize, nothing_run: bool, lang: Lang) -> String {
    let mut s = match lang {
        Lang::En => {
            let mut p = Vec::new();
            if errors > 0 {
                p.push(plural(errors, "error", "errors"));
            }
            if warnings > 0 {
                p.push(plural(warnings, "warning", "warnings"));
            }
            format!("{} found.", p.join(" and "))
        }
        Lang::De => {
            let mut p = Vec::new();
            if errors > 0 {
                p.push(plural(errors, "Fehler", "Fehler"));
            }
            if warnings > 0 {
                p.push(plural(warnings, "Warnung", "Warnungen"));
            }
            format!("{} gefunden.", p.join(" und "))
        }
    };
    if nothing_run {
        s.push_str(match lang {
            Lang::En => " Nothing was run.",
            Lang::De => " Es wurde nichts ausgeführt.",
        });
    }
    s
}

/// Compact list of several problems (Nutzerbeschreibung §3.10).
pub fn compact_list(ds: &[Diagnostic], src: &str, lang: Lang) -> String {
    let mut s = String::new();
    for (i, d) in ds.iter().enumerate() {
        let (line, _) = line_col(src, d.span.start);
        let word = match lang {
            Lang::En => "line",
            Lang::De => "Zeile",
        };
        let _ = writeln!(s, "  [{}] {word} {}:  {} [{}]", i + 1, line + 1, d.title.get(lang), d.code);
    }
    s
}

/// Sorts diagnostics by position, errors before warnings at the same position.
pub fn sort(ds: &mut [Diagnostic]) {
    ds.sort_by(|a, b| (a.span.start, a.severity, a.code).cmp(&(b.span.start, b.severity, b.code)));
}

/// Damerau-free Levenshtein distance for "did you mean" suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let c = if a[i - 1].eq_ignore_ascii_case(&b[j - 1]) { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + c);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest candidate within a sensible distance.
pub fn closest<'a>(word: &str, cands: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max = (word.chars().count() / 2).max(1) + 1;
    cands
        .into_iter()
        .map(|c| (edit_distance(word, c), c))
        .filter(|(d, c)| *d <= max && *c != word)
        .min_by_key(|(d, c)| (*d, c.len()))
        .map(|(_, c)| c)
}
