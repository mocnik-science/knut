//! Lexer (KNUT_LANG.md §2). Whitespace is significant: every token records whether
//! whitespace precedes and follows it, and the lexer reads tag keys and short-form tag
//! values (`A:amenity=library`) as their own tokens when the colon or equals sign is glued.

use crate::diag::{Diagnostic, Span, Text2};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Num(f64),
    /// A coordinate: a number glued to E, W, N or S (`8.8079E`, `53.07N`); W and S negative.
    /// The letter: `E` for longitudes, `N` for latitudes.
    Coord(f64, char),
    Str(String),
    /// `YYYY-M[-D][Thh:mm[:ss][Z|±hh:mm]]`
    Date(String),
    /// `7/3/2020` (always an error, E301)
    BadDate(String),
    /// Tag key after a glued colon (`name:de`).
    TagKey(String),
    /// Glued `=` between a tag key and its value.
    TagEq,
    /// Short-form tag value (`library`, `'Stadtbibliothek'`).
    TagValue(String),
    Colon,
    Dot,
    Ellipsis,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Assign,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Arrow,
    FatArrow,
    QQ,
    Newline,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("\"{s}\""),
            Tok::Num(n) => format!("the number {}", fmt_num(*n)),
            Tok::Coord(n, a) => format!("the coordinate {}{}", fmt_num(n.abs()), match (a, *n < 0.0) {
                ('E', false) => 'E',
                ('E', true) => 'W',
                (_, false) => 'N',
                _ => 'S',
            }),
            Tok::Str(s) => format!("the text '{s}'"),
            Tok::Date(s) | Tok::BadDate(s) => format!("the date {s}"),
            Tok::TagKey(s) => format!("the tag key \"{s}\""),
            Tok::TagValue(s) => format!("the tag value \"{s}\""),
            Tok::Newline => "the end of the line".into(),
            Tok::Eof => "the end of the script".into(),
            t => format!("\"{}\"", t.symbol()),
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            Tok::Colon => ":",
            Tok::Dot => ".",
            Tok::Ellipsis => "...",
            Tok::Comma => ",",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Assign | Tok::TagEq => "=",
            Tok::EqEq => "==",
            Tok::Ne => "!=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::QQ => "??",
            _ => "",
        }
    }
}

pub fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { format!("{n}") }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    pub sp_before: bool,
    pub sp_after: bool,
}

fn is_key_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_value_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'+' || c == b'-'
}

struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    p: usize,
    depth: i32,
    out: Vec<Token>,
    diags: Vec<Diagnostic>,
}

pub fn lex(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut l = Lexer { src, b: src.as_bytes(), p: 0, depth: 0, out: Vec::new(), diags: Vec::new() };
    l.run();
    let n = l.out.len();
    for i in 0..n {
        let start = l.out[i].span.start;
        let end = l.out[i].span.end;
        l.out[i].sp_before = i == 0 || l.out[i - 1].span.end < start || l.out[i - 1].tok == Tok::Newline;
        l.out[i].sp_after =
            i + 1 >= n || l.out[i + 1].span.start > end || matches!(l.out[i + 1].tok, Tok::Newline | Tok::Eof);
    }
    (l.out, l.diags)
}

impl<'a> Lexer<'a> {
    fn peek(&self, k: usize) -> u8 {
        *self.b.get(self.p + k).unwrap_or(&0)
    }

    fn push(&mut self, tok: Tok, start: usize) {
        self.out.push(Token { tok, span: Span::new(start, self.p), sp_before: false, sp_after: false });
    }

    fn last_glued(&self, start: usize) -> bool {
        self.out.last().is_some_and(|t| t.span.end as usize == start && t.tok != Tok::Newline)
    }

    /// Decides at a line break whether the statement continues (§2.1).
    fn newline(&mut self) {
        let start = self.p;
        self.p += 1;
        if self.depth > 0 {
            return;
        }
        // look at the next line
        let rest = &self.src[self.p..];
        let line = rest.split('\n').next().unwrap_or("");
        let trimmed = line.trim();
        let continues = !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && (line.starts_with(' ') || line.starts_with('\t'));
        if !continues && !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
            self.out.push(Token { tok: Tok::Newline, span: Span::new(start, start + 1), sp_before: true, sp_after: true });
        }
    }

    fn run(&mut self) {
        while self.p < self.b.len() {
            let c = self.b[self.p];
            let start = self.p;
            match c {
                b'\n' => self.newline(),
                b' ' | b'\t' | b'\r' => self.p += 1,
                b'#' => {
                    while self.p < self.b.len() && self.b[self.p] != b'\n' {
                        self.p += 1;
                    }
                }
                b'\'' | b'"' => {
                    let s = self.string(c);
                    self.push(Tok::Str(s), start);
                }
                b'0'..=b'9' => self.number(),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                    while self.p < self.b.len() && is_key_char(self.b[self.p]) {
                        self.p += 1;
                    }
                    // `reverse-geocode`, `reverse-geocoded`: one word
                    if &self.src[start..self.p] == "reverse" && self.src[self.p..].starts_with("-geocode") {
                        let end = self.p + "-geocode".len();
                        let end = if self.b.get(end) == Some(&b'd') { end + 1 } else { end };
                        if !self.b.get(end).is_some_and(|c| is_key_char(*c)) {
                            self.p = end;
                        }
                    }
                    let s = self.src[start..self.p].to_string();
                    self.push(Tok::Ident(s), start);
                }
                b':' => {
                    let glued_left = self.last_glued(start);
                    self.p += 1;
                    self.push(Tok::Colon, start);
                    if glued_left && is_key_char(self.peek(0)) {
                        self.tag_key();
                    }
                }
                b'.' => {
                    if self.peek(1) == b'.' && self.peek(2) == b'.' {
                        self.p += 3;
                        self.push(Tok::Ellipsis, start);
                    } else {
                        self.p += 1;
                        self.push(Tok::Dot, start);
                    }
                }
                b',' => {
                    self.p += 1;
                    self.push(Tok::Comma, start);
                }
                b'(' | b'[' => {
                    self.p += 1;
                    self.depth += 1;
                    self.push(if c == b'(' { Tok::LParen } else { Tok::LBracket }, start);
                }
                b')' | b']' => {
                    self.p += 1;
                    self.depth = (self.depth - 1).max(0);
                    self.push(if c == b')' { Tok::RParen } else { Tok::RBracket }, start);
                }
                b'=' => {
                    if self.peek(1) == b'=' {
                        self.p += 2;
                        self.push(Tok::EqEq, start);
                    } else if self.peek(1) == b'>' {
                        self.p += 2;
                        self.push(Tok::FatArrow, start);
                    } else {
                        self.p += 1;
                        self.push(Tok::Assign, start);
                    }
                }
                b'!' if self.peek(1) == b'=' => {
                    self.p += 2;
                    self.push(Tok::Ne, start);
                }
                b'<' => {
                    if self.peek(1) == b'=' {
                        self.p += 2;
                        self.push(Tok::Le, start);
                    } else {
                        self.p += 1;
                        self.push(Tok::Lt, start);
                    }
                }
                b'>' => {
                    if self.peek(1) == b'=' {
                        self.p += 2;
                        self.push(Tok::Ge, start);
                    } else {
                        self.p += 1;
                        self.push(Tok::Gt, start);
                    }
                }
                b'+' => {
                    self.p += 1;
                    self.push(Tok::Plus, start);
                }
                b'-' => {
                    if self.peek(1) == b'>' {
                        self.p += 2;
                        self.push(Tok::Arrow, start);
                    } else {
                        self.p += 1;
                        self.push(Tok::Minus, start);
                    }
                }
                b'*' => {
                    self.p += 1;
                    self.push(Tok::Star, start);
                }
                b'/' => {
                    self.p += 1;
                    self.push(Tok::Slash, start);
                }
                b'%' => {
                    self.p += 1;
                    self.push(Tok::Percent, start);
                }
                b'?' if self.peek(1) == b'?' => {
                    self.p += 2;
                    self.push(Tok::QQ, start);
                }
                _ => {
                    let ch = self.src[self.p..].chars().next().unwrap();
                    self.p += ch.len_utf8();
                    self.diags.push(
                        Diagnostic::error(
                            "E108",
                            Span::new(start, self.p),
                            Text2::new(format!("The sign \"{ch}\" is not part of the language."), format!("Das Zeichen \"{ch}\" gehört nicht zur Sprache.")),
                        )
                        .why(
                            "Names, keys and values use letters, digits and '_'. Other text goes in quotes.",
                            "Namen, Schlüssel und Werte bestehen aus Buchstaben, Ziffern und '_'. Anderer Text steht in Anführungszeichen.",
                        ),
                    );
                }
            }
        }
        let end = self.b.len();
        if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
            self.out.push(Token { tok: Tok::Newline, span: Span::new(end, end), sp_before: true, sp_after: true });
        }
        self.out.push(Token { tok: Tok::Eof, span: Span::new(end, end), sp_before: true, sp_after: true });
    }

    fn string(&mut self, q: u8) -> String {
        let start = self.p;
        self.p += 1;
        let mut s = String::new();
        loop {
            if self.p >= self.b.len() || self.b[self.p] == b'\n' {
                let qc = q as char;
                self.diags.push(
                    Diagnostic::error(
                        "E107",
                        Span::new(start, self.p),
                        Text2::new(format!("A text is not closed (missing {qc})."), format!("Ein Text ist nicht abgeschlossen (es fehlt {qc}).")),
                    )
                    .why("Texts start and end with the same quote on one line.", "Texte beginnen und enden in derselben Zeile mit demselben Anführungszeichen."),
                );
                return s;
            }
            let c = self.b[self.p];
            if c == q {
                self.p += 1;
                return s;
            }
            if c == b'\\' && self.p + 1 < self.b.len() {
                let e = self.b[self.p + 1];
                s.push(match e {
                    b'n' => '\n',
                    b't' => '\t',
                    other => other as char,
                });
                self.p += 2;
                continue;
            }
            let ch = self.src[self.p..].chars().next().unwrap();
            s.push(ch);
            self.p += ch.len_utf8();
        }
    }

    fn digits(&mut self) -> usize {
        let s = self.p;
        while self.p < self.b.len() && self.b[self.p].is_ascii_digit() {
            self.p += 1;
        }
        self.p - s
    }

    fn number(&mut self) {
        let start = self.p;
        let n = self.digits();
        // 7/3/2020
        if n <= 2 && self.peek(0) == b'/' && self.peek(1).is_ascii_digit() {
            let save = self.p;
            self.p += 1;
            let m = self.digits();
            if m <= 2 && self.peek(0) == b'/' && self.peek(1).is_ascii_digit() {
                self.p += 1;
                self.digits();
                let s = self.src[start..self.p].to_string();
                self.push(Tok::BadDate(s), start);
                return;
            }
            self.p = save;
        }
        // YYYY-M[-D][T…]
        if n == 4 && self.peek(0) == b'-' && self.peek(1).is_ascii_digit() {
            let save = self.p;
            self.p += 1;
            let m = self.digits();
            if (1..=2).contains(&m) {
                if self.peek(0) == b'-' && self.peek(1).is_ascii_digit() {
                    self.p += 1;
                    let d = self.digits();
                    if !(1..=2).contains(&d) {
                        self.p = save;
                        return self.finish_number(start);
                    }
                    if self.peek(0) == b'T' && self.peek(1).is_ascii_digit() {
                        self.p += 1;
                        self.digits();
                        while self.peek(0) == b':' && self.peek(1).is_ascii_digit() {
                            self.p += 1;
                            self.digits();
                        }
                        if self.peek(0) == b'Z' {
                            self.p += 1;
                        } else if (self.peek(0) == b'+' || self.peek(0) == b'-') && self.peek(1).is_ascii_digit() {
                            self.p += 1;
                            self.digits();
                            if self.peek(0) == b':' {
                                self.p += 1;
                                self.digits();
                            }
                        }
                    }
                }
                let s = self.src[start..self.p].to_string();
                self.push(Tok::Date(s), start);
                return;
            }
            self.p = save;
        }
        self.finish_number(start);
    }

    fn finish_number(&mut self, start: usize) {
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            self.p += 1;
            self.digits();
        }
        let v: f64 = self.src[start..self.p].parse().unwrap_or(0.0);
        // a coordinate: `8.8079E`, `53.07N` (the letter glued, no letter or digit after it)
        let axis = self.peek(0);
        if matches!(axis, b'E' | b'W' | b'N' | b'S') && !is_key_char(self.peek(1)) {
            self.p += 1;
            let (v, a) = match axis {
                b'E' => (v, 'E'),
                b'W' => (-v, 'E'),
                b'N' => (v, 'N'),
                _ => (-v, 'N'),
            };
            self.push(Tok::Coord(v, a), start);
            return;
        }
        self.push(Tok::Num(v), start);
    }

    fn tag_key(&mut self) {
        let start = self.p;
        loop {
            while self.p < self.b.len() && is_key_char(self.b[self.p]) {
                self.p += 1;
            }
            if self.peek(0) == b':' && is_key_char(self.peek(1)) {
                self.p += 1;
                continue;
            }
            break;
        }
        let k = self.src[start..self.p].to_string();
        self.push(Tok::TagKey(k), start);
        // short form `=value` (not `==`, not `=>`, value glued)
        if self.peek(0) == b'=' && self.peek(1) != b'=' && self.peek(1) != b'>' {
            let v0 = self.peek(1);
            if v0 == b'\'' || v0 == b'"' {
                let s = self.p;
                self.p += 1;
                self.push(Tok::TagEq, s);
                let vs = self.p;
                let v = self.string(v0);
                self.push(Tok::TagValue(v), vs);
            } else if is_value_char(v0) {
                let s = self.p;
                self.p += 1;
                self.push(Tok::TagEq, s);
                let vs = self.p;
                while self.p < self.b.len() && is_value_char(self.b[self.p]) {
                    self.p += 1;
                }
                if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
                    self.p += 1;
                    self.digits();
                }
                let v = self.src[vs..self.p].to_string();
                self.push(Tok::TagValue(v), vs);
            }
        }
    }
}
