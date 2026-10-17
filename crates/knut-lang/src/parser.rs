//! Recursive-descent parser (KNUT_LANG.md §3). Errors are collected per statement; after
//! an error the parser skips to the next statement, so all problems are reported at once.

use crate::ast::*;
use crate::dates::{self, TimeUnit};
use crate::diag::{Diagnostic, Header, Span, Text2};
use crate::lexer::{Tok, Token, lex};
use crate::units;

pub const SETTINGS: [&str; 5] = ["knut_db", "seed", "messages", "ignore_warnings", "near_distance"];

/// Reserved words (§2.4).
pub const KEYWORDS: &[&str] = &[
    "with", "within", "on", "from", "to", "before", "after", "between", "and", "or", "not", "in", "pick", "exactly",
    "randomly", "using", "seed", "the", "all", "some", "no", "nearest", "of", "by", "per", "having", "closer", "than",
    "equal", "near", "overlapping", "next", "previous", "define", "split", "at", "each", "path", "isochrone",
    "dataset", "graph", "groups", "ordered", "none", "true", "false", "geocode", "geocoded", "reverse-geocode",
    "reverse-geocoded",
];

type PResult<T> = Result<T, ()>;

pub struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    p: usize,
    pub diags: Vec<Diagnostic>,
    /// Inside a `with` condition: `within` binds only to paths (§3.4).
    in_cond: bool,
    /// Parsing the operand of an `of` call: nested `of` calls take no clauses (§3.5).
    inner_of: bool,
    stmt_start: usize,
}

/// Parses a whole script. Returns the AST of all statements that parsed and all
/// diagnostics found by the lexer and parser.
pub fn parse(src: &str) -> (Script, Vec<Diagnostic>) {
    let (toks, mut diags) = lex(src);
    let mut p = Parser { src, toks, p: 0, diags: Vec::new(), in_cond: false, inner_of: false, stmt_start: 0 };
    let mut stmts = Vec::new();
    loop {
        while p.tok() == &Tok::Newline {
            p.p += 1;
        }
        if p.tok() == &Tok::Eof {
            break;
        }
        p.stmt_start = p.span().start as usize;
        let start = p.p;
        match p.statement() {
            Ok(s) => {
                if p.tok() != &Tok::Newline && p.tok() != &Tok::Eof {
                    p.unexpected("the end of the line", "das Zeilenende");
                    p.skip_line();
                } else {
                    stmts.push(s);
                }
            }
            Err(()) => {
                if p.p == start {
                    p.p += 1;
                }
                p.skip_line();
            }
        }
    }
    diags.append(&mut p.diags);
    (Script { stmts }, diags)
}

fn kw_pair(en: &str) -> (String, String) {
    (en.to_string(), en.to_string())
}

impl<'a> Parser<'a> {
    // ----- token helpers -------------------------------------------------------------

    fn tok(&self) -> &Tok {
        &self.toks[self.p].tok
    }

    fn tok_at(&self, k: usize) -> &Tok {
        &self.toks[(self.p + k).min(self.toks.len() - 1)].tok
    }

    fn t(&self) -> &Token {
        &self.toks[self.p]
    }

    fn t_at(&self, k: usize) -> &Token {
        &self.toks[(self.p + k).min(self.toks.len() - 1)]
    }

    fn span(&self) -> Span {
        self.toks[self.p].span
    }

    fn prev_span(&self) -> Span {
        self.toks[self.p.saturating_sub(1)].span
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.p].clone();
        if self.p + 1 < self.toks.len() {
            self.p += 1;
        }
        t
    }

    fn is_kw(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Ident(x) if x == s)
    }

    fn is_kw_at(&self, k: usize, s: &str) -> bool {
        matches!(self.tok_at(k), Tok::Ident(x) if x == s)
    }

    fn eat_kw(&mut self, s: &str) -> bool {
        if self.is_kw(s) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, s: &str) -> PResult<Span> {
        if self.is_kw(s) {
            Ok(self.bump().span)
        } else {
            let (en, de) = kw_pair(&format!("\"{s}\""));
            self.unexpected(&en, &de);
            Err(())
        }
    }

    fn expect(&mut self, t: Tok) -> PResult<Span> {
        if *self.tok() == t {
            Ok(self.bump().span)
        } else {
            let s = format!("\"{}\"", t.symbol());
            self.unexpected(&s, &s);
            Err(())
        }
    }

    /// Expects the bracket that closes `open`. If the script continues on another line or
    /// ends instead, the opening bracket is reported (E109).
    fn close(&mut self, t: Tok, open: Span) -> PResult<Span> {
        if *self.tok() == t {
            return Ok(self.bump().span);
        }
        let cur = self.span();
        let later = matches!(self.tok(), Tok::Eof) || self.src.get(open.end as usize..cur.start as usize).is_some_and(|x| x.contains('\n'));
        if !later {
            return self.expect(t);
        }
        let sym = t.symbol();
        self.diags.push(
            Diagnostic::error("E109", open, Text2::new("This bracket is not closed.", "Diese Klammer wird nicht geschlossen."))
                .why(
                    format!("A \"{sym}\" is missing. Until it comes, the following lines count as part of this statement."),
                    format!("Es fehlt ein \"{sym}\". Bis dahin gelten die folgenden Zeilen als Teil dieses Statements."),
                ),
        );
        Err(())
    }

    fn ident(&mut self) -> PResult<(String, Span)> {
        if let Tok::Ident(s) = self.tok().clone() {
            let sp = self.bump().span;
            Ok((s, sp))
        } else {
            self.unexpected("a name", "ein Name");
            Err(())
        }
    }

    fn text(&self, s: Span) -> &'a str {
        &self.src[s.start as usize..(s.end as usize).min(self.src.len())]
    }

    fn skip_line(&mut self) {
        while !matches!(self.tok(), Tok::Newline | Tok::Eof) {
            self.p += 1;
        }
    }

    fn line_end(&self) -> usize {
        let mut k = self.p;
        while !matches!(self.toks[k].tok, Tok::Newline | Tok::Eof) {
            k += 1;
        }
        if k > 0 { self.toks[k - 1].span.end as usize } else { 0 }
    }

    fn unexpected(&mut self, exp_en: &str, exp_de: &str) {
        let t = self.t().clone();
        let what = t.tok.describe();
        let (title_en, title_de) = match &t.tok {
            Tok::Newline | Tok::Eof => (
                format!("The line ends too early: {exp_en} is missing."),
                format!("Die Zeile endet zu früh: Es fehlt {exp_de}."),
            ),
            _ => (format!("Unexpected {what} here."), format!("Unerwartet: {what}.")),
        };
        if matches!(t.tok, Tok::RParen | Tok::RBracket) {
            self.diags.push(Diagnostic::error("E109", t.span, Text2::new("This bracket closes nothing.", "Diese Klammer schließt nichts.")).why(
                format!("Expected {exp_en}. Is there one closing bracket too many?"),
                format!("Erwartet wurde {exp_de}. Ist eine schließende Klammer zu viel?"),
            ));
            return;
        }
        let mut d = Diagnostic::error("E108", t.span, Text2::new(title_en, title_de));
        if !matches!(t.tok, Tok::Newline | Tok::Eof) {
            d = d.why(format!("Expected {exp_en}."), format!("Erwartet wurde {exp_de}."));
        }
        if let Tok::Ident(w) = &t.tok
            && KEYWORDS.contains(&w.as_str())
        {
            d = d.why(
                format!("\"{w}\" is a keyword and cannot stand here."),
                format!("\"{w}\" ist ein Schlüsselwort und kann hier nicht stehen."),
            );
        }
        self.diags.push(d);
    }

    // ----- statements -----------------------------------------------------------------

    fn statement(&mut self) -> PResult<Stmt> {
        let start = self.span();
        if let Tok::Ident(name) = self.tok().clone() {
            if name == "define" {
                return self.define();
            }
            if *self.tok_at(1) == Tok::Assign {
                let name_span = self.bump().span;
                self.bump();
                let value = self.expr()?;
                let span = start.to(value.span);
                let kind = if SETTINGS.contains(&name.as_str()) {
                    StmtKind::Setting { name, name_span, value }
                } else {
                    StmtKind::Assign { name, name_span, value }
                };
                return Ok(Stmt { kind, span });
            }
        }
        let mut outs = vec![self.expr()?];
        while *self.tok() == Tok::Comma {
            self.bump();
            outs.push(self.expr()?);
        }
        let span = start.to(outs.last().unwrap().span);
        Ok(Stmt { kind: StmtKind::Output(outs), span })
    }

    fn define(&mut self) -> PResult<Stmt> {
        let start = self.bump().span;
        let kind = if *self.tok() == Tok::Star {
            self.bump();
            Kind::Any
        } else {
            let (k, sp) = self.ident()?;
            match Kind::parse(&k) {
                Some((k, _)) => k,
                None => {
                    self.diags.push(
                        Diagnostic::error(
                            "E108",
                            sp,
                            Text2::new(format!("\"{k}\" is not a kind of object."), format!("\"{k}\" ist keine Objektsorte.")),
                        )
                        .why(
                            "After \"define\" comes node, way, area, relation, *, changeset, user, note or key.",
                            "Nach \"define\" folgt node, way, area, relation, *, changeset, user, note oder key.",
                        ),
                    );
                    return Err(());
                }
            }
        };
        let (var, _) = self.ident()?;
        if *self.tok() != Tok::Colon {
            self.unexpected("\":\"", "\":\"");
            return Err(());
        }
        self.bump();
        let (field, field_span) = match self.tok().clone() {
            Tok::TagKey(k) => (k, self.bump().span),
            _ => self.ident()?,
        };
        self.expect(Tok::Assign)?;
        let body = self.expr()?;
        let span = start.to(body.span);
        Ok(Stmt { kind: StmtKind::Define { kind, var, field, field_span, body }, span })
    }

    // ----- expressions -----------------------------------------------------------------

    pub fn expr(&mut self) -> PResult<Expr> {
        if let Some(l) = self.try_lambda()? {
            return Ok(l);
        }
        self.fallback()
    }

    fn try_lambda(&mut self) -> PResult<Option<Expr>> {
        let start = self.span();
        if let Tok::Ident(n) = self.tok().clone()
            && *self.tok_at(1) == Tok::FatArrow
        {
            self.bump();
            self.bump();
            let body = self.expr()?;
            let span = start.to(body.span);
            return Ok(Some(Expr::new(ExprKind::Lambda(vec![n], Box::new(body)), span)));
        }
        if *self.tok() == Tok::LParen {
            let mut k = 1;
            let mut names = Vec::new();
            loop {
                match self.tok_at(k) {
                    Tok::Ident(n) => names.push(n.clone()),
                    _ => return Ok(None),
                }
                k += 1;
                match self.tok_at(k) {
                    Tok::Comma => k += 1,
                    Tok::RParen => break,
                    _ => return Ok(None),
                }
            }
            if *self.tok_at(k + 1) != Tok::FatArrow {
                return Ok(None);
            }
            self.p += k + 2;
            let body = self.expr()?;
            let span = start.to(body.span);
            return Ok(Some(Expr::new(ExprKind::Lambda(names, Box::new(body)), span)));
        }
        Ok(None)
    }

    fn fallback(&mut self) -> PResult<Expr> {
        let mut l = self.disj()?;
        while *self.tok() == Tok::QQ {
            self.bump();
            let r = self.disj()?;
            let sp = l.span.to(r.span);
            l = Expr::new(ExprKind::Fallback(Box::new(l), Box::new(r)), sp);
        }
        Ok(l)
    }

    fn disj(&mut self) -> PResult<Expr> {
        let mut l = self.conj()?;
        while self.is_kw("or") {
            self.bump();
            let r = self.conj()?;
            let sp = l.span.to(r.span);
            l = Expr::new(ExprKind::Or(Box::new(l), Box::new(r)), sp);
        }
        Ok(l)
    }

    fn conj(&mut self) -> PResult<Expr> {
        let mut l = self.neg()?;
        while self.is_kw("and") {
            self.bump();
            let r = self.neg()?;
            let sp = l.span.to(r.span);
            l = Expr::new(ExprKind::And(Box::new(l), Box::new(r)), sp);
        }
        Ok(l)
    }

    fn neg(&mut self) -> PResult<Expr> {
        if self.is_kw("not") {
            let s = self.bump().span;
            let e = self.neg()?;
            let sp = s.to(e.span);
            return Ok(Expr::new(ExprKind::Not(Box::new(e)), sp));
        }
        self.relation()
    }

    fn cmp_op(&self) -> Option<CmpOp> {
        Some(match self.tok() {
            Tok::EqEq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            _ => return None,
        })
    }

    /// True if the current token starts a relational suffix.
    fn at_rel_tail(&self) -> bool {
        self.cmp_op().is_some()
            || *self.tok() == Tok::Assign
            || self.is_kw("in")
            || self.is_kw("between")
            || self.is_kw("within")
            || self.is_kw("overlapping")
            || self.is_kw("near")
            || self.is_kw("before")
            || self.is_kw("after")
            || ((self.is_kw("next") || self.is_kw("previous")) && self.is_kw_at(1, "of"))
            || (self.is_kw("not") && ["in", "within", "between", "overlapping"].iter().any(|k| self.is_kw_at(1, k)))
    }

    fn relation(&mut self) -> PResult<Expr> {
        let l = self.additive()?;
        if self.is_kw("within") && self.in_cond && !l.is_elem_path() {
            // `within` ends the condition and belongs to the selection (§3.4)
            return Ok(l);
        }
        if !self.at_rel_tail() {
            return Ok(l);
        }
        self.rel_tail(l)
    }

    fn rel_tail(&mut self, l: Expr) -> PResult<Expr> {
        if *self.tok() == Tok::Assign {
            let eq = self.bump().span;
            let r = self.additive();
            let lt = self.text(l.span).to_string();
            let mut d = Diagnostic::error(
                "E101",
                eq,
                Text2::new("This looks like an assignment, not a comparison.", "Das sieht nach einer Zuweisung aus, nicht nach einem Vergleich."),
            )
            .why(
                "A single '=' with spaces around it assigns a value, which does not\nmake sense inside a condition.",
                "Ein einzelnes '=' mit Leerzeichen weist einen Wert zu, das ergibt\nin einer Bedingung keinen Sinn.",
            )
            .header(Header::DidYouMeanOne);
            if let Ok(r) = &r {
                let rt = self.text(r.span).to_string();
                if l.is_path() && matches!(l.kind, ExprKind::Tag(..)) {
                    d = d.sugg_note(
                        format!("{lt}={rt}"),
                        "no spaces: short form for \"is equal to\"",
                        "ohne Leerzeichen: Kurzform für \"ist gleich\"",
                    );
                }
                d = d.sugg_note(format!("{lt} == {rt}"), "with spaces: comparison", "mit Leerzeichen: Vergleich");
            }
            self.diags.push(d);
            return Err(());
        }
        // `x not in S` = `x != none and not (x in S)`: like `!=`, false if x is missing (§5.3)
        if self.is_kw("not") {
            let not_span = self.bump().span;
            let present = Expr::new(ExprKind::Rel(Box::new(l.clone()), RelTail::Cmp(CmpOp::Ne, Box::new(Expr::new(ExprKind::None, not_span)))), l.span);
            let inner = self.rel_tail(l)?;
            let sp = inner.span;
            let not = Expr::new(ExprKind::Not(Box::new(inner)), sp);
            return Ok(Expr::new(ExprKind::And(Box::new(present), Box::new(not)), sp));
        }
        if let Some(op) = self.cmp_op() {
            self.bump();
            let r = self.additive()?;
            let sp = l.span.to(r.span);
            return Ok(Expr::new(ExprKind::Rel(Box::new(l), RelTail::Cmp(op, Box::new(r))), sp));
        }
        let kw = match self.tok() {
            Tok::Ident(s) => s.clone(),
            _ => unreachable!(),
        };
        self.bump();
        let tail = match kw.as_str() {
            "in" => RelTail::In(Box::new(self.additive()?)),
            "between" => {
                let a = self.additive()?;
                self.expect_kw("and")?;
                let b = self.additive()?;
                RelTail::Between(Box::new(a), Box::new(b))
            }
            "within" => RelTail::Within(Box::new(self.region()?)),
            "overlapping" => RelTail::Overlapping(Box::new(self.additive()?)),
            "before" => RelTail::Before(Box::new(self.additive()?)),
            "after" => RelTail::After(Box::new(self.additive()?)),
            "next" | "previous" => {
                self.expect_kw("of")?;
                let a = self.additive()?;
                if kw == "next" { RelTail::NextOf(Box::new(a)) } else { RelTail::PreviousOf(Box::new(a)) }
            }
            "near" => {
                let lt = self.text(l.span).to_string();
                self.near(&lt)?
            }
            _ => unreachable!(),
        };
        let end = self.prev_span();
        let sp = l.span.to(end);
        Ok(Expr::new(ExprKind::Rel(Box::new(l), tail), sp))
    }

    fn near(&mut self, lt: &str) -> PResult<RelTail> {
        let quant = if self.eat_kw("some") {
            Quant::Some
        } else if self.eat_kw("no") {
            Quant::No
        } else if self.eat_kw("all") {
            Quant::All
        } else {
            self.unexpected("\"some\", \"no\" or \"all\"", "\"some\", \"no\" oder \"all\"");
            return Err(());
        };
        if !self.is_kw("_") {
            self.unexpected("\"_\"", "\"_\"");
            return Err(());
        }
        self.bump();
        if self.is_kw("closer") {
            // E105: threshold before the set
            let c0 = self.span();
            let th = self.threshold()?;
            let c1 = self.prev_span();
            if self.is_kw("in") {
                self.bump();
                let set = self.unary()?;
                let quant_text = match quant {
                    Quant::Some => "some",
                    Quant::No => "no",
                    Quant::All => "all",
                };
                let fixed = format!(
                    "{lt} near {quant_text} _ in {} {}",
                    self.text(set.span),
                    self.text(c0.to(c1))
                );
                let _ = th;
                self.diags.push(
                    Diagnostic::error(
                        "E105",
                        c0.to(c1),
                        Text2::new("\"closer than\" comes after the set, not before it.", "\"closer than\" steht hinter der Menge, nicht davor."),
                    )
                    .header(Header::DidYouMean)
                    .sugg(fixed),
                );
            } else {
                self.diags.push(
                    Diagnostic::error(
                        "E501",
                        self.span(),
                        Text2::new("\"near some\" needs a set after \"in\".", "\"near some\" braucht nach \"in\" eine Menge."),
                    )
                    .header(Header::ForExample)
                    .why("Say what you are looking for, for example:", "Sag, wonach gesucht wird, zum Beispiel:")
                    .sugg("h near some _ in schools closer than 300 m"),
                );
            }
            return Err(());
        }
        if !self.is_kw("in") {
            let mut d = Diagnostic::error(
                "E501",
                self.prev_span(),
                Text2::new("\"near some\" needs a set after \"in\".", "\"near some\" braucht nach \"in\" eine Menge."),
            )
            .why("Say what you are looking for, for example:", "Sag, wonach gesucht wird, zum Beispiel:");
            d = d.sugg("h near some _ in schools closer than 300 m");
            self.diags.push(d);
            return Err(());
        }
        self.bump();
        let set = self.unary()?;
        let threshold = if self.is_kw("closer") { Some(self.threshold()?) } else { None };
        Ok(RelTail::Near { quant, set: Box::new(set), threshold })
    }

    fn threshold(&mut self) -> PResult<Threshold> {
        let s = self.expect_kw("closer")?;
        self.expect_kw("than")?;
        let or_equal = if self.is_kw("or") && self.is_kw_at(1, "equal") {
            self.bump();
            self.bump();
            true
        } else {
            false
        };
        let value = self.additive()?;
        let mode = if self.is_kw("by") { Some(self.mode_clause()?) } else { None };
        let span = s.to(self.prev_span());
        Ok(Threshold { value: Box::new(value), or_equal, mode, span })
    }

    fn mode_clause(&mut self) -> PResult<ModeClause> {
        self.expect_kw("by")?;
        let (m, sp) = self.ident()?;
        let Some(mode) = Mode::parse(&m) else {
            let mut d = Diagnostic::error(
                "E108",
                sp,
                Text2::new(format!("\"{m}\" is not a way of moving."), format!("\"{m}\" ist keine Fortbewegungsart.")),
            )
            .why("Possible are foot, bicycle, car and transit.", "Möglich sind foot, bicycle, car und transit.");
            if let Some(c) = crate::diag::closest(&m, ["foot", "bicycle", "car", "transit"]) {
                d = d.header(Header::DidYouMean).sugg(format!("by {c}"));
            }
            self.diags.push(d);
            return Err(());
        };
        let using = if self.eat_kw("using") { Some(Box::new(self.unary()?)) } else { None };
        Ok(ModeClause { mode, mode_span: sp, using })
    }

    fn additive(&mut self) -> PResult<Expr> {
        let mut l = self.term()?;
        loop {
            let op = match self.tok() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let r = self.term()?;
            let sp = l.span.to(r.span);
            l = Expr::new(ExprKind::Bin(op, Box::new(l), Box::new(r)), sp);
        }
        Ok(l)
    }

    fn term(&mut self) -> PResult<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.tok() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                _ => break,
            };
            self.bump();
            let r = self.unary()?;
            let sp = l.span.to(r.span);
            l = Expr::new(ExprKind::Bin(op, Box::new(l), Box::new(r)), sp);
        }
        Ok(l)
    }

    fn unary(&mut self) -> PResult<Expr> {
        if *self.tok() == Tok::Minus {
            let s = self.bump().span;
            if matches!(self.tok(), Tok::Num(_)) && *self.tok_at(1) == Tok::Ellipsis {
                return self.box_lit(Some(s));
            }
            let e = self.unary()?;
            let sp = s.to(e.span);
            if let ExprKind::Num(v) = e.kind {
                return Ok(Expr::new(ExprKind::Num(-v), sp));
            }
            if let ExprKind::Unit(inner, u, us) = &e.kind
                && let ExprKind::Num(v) = inner.kind
            {
                return Ok(Expr::new(ExprKind::Unit(Box::new(Expr::new(ExprKind::Num(-v), sp)), u, *us), sp));
            }
            return Ok(Expr::new(ExprKind::Neg(Box::new(e)), sp));
        }
        self.quantity()
    }

    /// Unit after the current token, if any: `(unit, span, tokens)`.
    fn peek_unit(&self) -> Option<(&'static units::Unit, Span, usize)> {
        let t = self.t();
        if t.tok == Tok::Percent {
            return Some((units::unit("%").unwrap(), t.span, 1));
        }
        let Tok::Ident(w) = &t.tok else { return None };
        if *self.tok_at(1) == Tok::FatArrow {
            return None;
        }
        if w == "km" && *self.tok_at(1) == Tok::Slash && !self.t_at(1).sp_before && self.is_kw_at(2, "h") && !self.t_at(2).sp_before {
            return Some((units::unit("km/h").unwrap(), t.span.to(self.t_at(2).span), 3));
        }
        units::unit(w).map(|u| (u, t.span, 1))
    }

    fn quantity(&mut self) -> PResult<Expr> {
        let e = self.postfix()?;
        let unit_ok = matches!(e.kind, ExprKind::Num(_)) || matches!(e.kind, ExprKind::Tag(_, _, None));
        if unit_ok && let Some((u, sp, n)) = self.peek_unit() {
            self.p += n;
            let span = e.span.to(sp);
            return Ok(Expr::new(ExprKind::Unit(Box::new(e), u, sp), span));
        }
        Ok(e)
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            match self.tok().clone() {
                Tok::Dot if !self.t().sp_before => {
                    self.bump();
                    let (f, fs) = match self.tok().clone() {
                        Tok::Ident(f) if !self.t().sp_before => (f, self.bump().span),
                        _ => {
                            self.unexpected("a field name directly after the dot", "ein Feldname direkt nach dem Punkt");
                            return Err(());
                        }
                    };
                    let sp = e.span.to(fs);
                    e = Expr::new(ExprKind::Field(Box::new(e), f, fs), sp);
                }
                Tok::Colon => {
                    let ct = self.t().clone();
                    if ct.sp_before || ct.sp_after {
                        let lt = self.text(e.span).to_string();
                        self.bump();
                        let rt = match self.tok() {
                            Tok::Ident(s) | Tok::TagKey(s) => s.clone(),
                            _ => String::new(),
                        };
                        let mut d = Diagnostic::error(
                            "E102",
                            ct.span,
                            Text2::new("A colon with spaces around it is not allowed.", "Ein Doppelpunkt mit Leerzeichen ist nicht erlaubt."),
                        )
                        .why(
                            "A colon reads a tag only when it touches both sides.",
                            "Ein Doppelpunkt liest nur dann ein Tag, wenn er auf beiden Seiten anliegt.",
                        );
                        if !rt.is_empty() {
                            d = d.header(Header::DidYouMean).sugg(format!("{lt}:{rt}"));
                        }
                        self.diags.push(d);
                        return Err(());
                    }
                    self.bump();
                    match self.tok().clone() {
                        Tok::TagKey(k) => {
                            let mut end = self.bump().span;
                            let mut value = None;
                            if *self.tok() == Tok::TagEq {
                                self.bump();
                                if let Tok::TagValue(v) = self.tok().clone() {
                                    end = self.bump().span;
                                    value = Some(v);
                                }
                            }
                            let sp = e.span.to(end);
                            e = Expr::new(ExprKind::Tag(Box::new(e), TagKey::Lit(k), value), sp);
                        }
                        Tok::LBracket => {
                            let open = self.bump().span;
                            let save = self.inner_of;
                            self.inner_of = false;
                            let k = self.expr();
                            self.inner_of = save;
                            let k = k?;
                            let end = self.close(Tok::RBracket, open)?;
                            let key = match k.kind {
                                ExprKind::Str(s) => TagKey::Lit(s),
                                _ => TagKey::Expr(Box::new(k)),
                            };
                            let sp = e.span.to(end);
                            e = Expr::new(ExprKind::Tag(Box::new(e), key, None), sp);
                        }
                        _ => {
                            self.unexpected("a tag key", "ein Tag-Schlüssel");
                            return Err(());
                        }
                    }
                }
                Tok::LParen if !self.t().sp_before => {
                    self.bump();
                    let save = (self.inner_of, self.in_cond);
                    self.inner_of = false;
                    self.in_cond = false;
                    let mut args = Vec::new();
                    let r: PResult<()> = (|| {
                        if *self.tok() != Tok::RParen {
                            args.push(self.expr()?);
                            while *self.tok() == Tok::Comma {
                                self.bump();
                                args.push(self.expr()?);
                            }
                        }
                        Ok(())
                    })();
                    (self.inner_of, self.in_cond) = save;
                    r?;
                    let end = self.expect(Tok::RParen)?;
                    let sp = e.span.to(end);
                    e = Expr::new(ExprKind::Call(Box::new(e), args), sp);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn box_lit(&mut self, neg0: Option<Span>) -> PResult<Expr> {
        let start = neg0.unwrap_or(self.span());
        let num = |p: &mut Parser, first: bool| -> PResult<f64> {
            let mut sign = 1.0;
            if !(first && neg0.is_some()) && *p.tok() == Tok::Minus {
                p.bump();
                sign = -1.0;
            }
            if first && neg0.is_some() {
                sign = -1.0;
            }
            match p.tok().clone() {
                Tok::Num(v) => {
                    p.bump();
                    Ok(sign * v)
                }
                _ => {
                    p.unexpected("a number", "eine Zahl");
                    Err(())
                }
            }
        };
        let a = num(self, true)?;
        self.expect(Tok::Ellipsis)?;
        let b = num(self, false)?;
        if *self.tok() != Tok::Comma {
            self.unexpected("\",\" and the range of latitudes", "\",\" und der Bereich der Breiten");
            return Err(());
        }
        self.bump();
        let c = num(self, false)?;
        self.expect(Tok::Ellipsis)?;
        let d = num(self, false)?;
        let sp = start.to(self.prev_span());
        if !(-180.0..=180.0).contains(&a) || !(-180.0..=180.0).contains(&b) || !(-90.0..=90.0).contains(&c) || !(-90.0..=90.0).contains(&d) {
            self.diags.push(
                Diagnostic::error(
                    "E108",
                    sp,
                    Text2::new("This box lies outside the world.", "Diese Box liegt außerhalb der Welt."),
                )
                .why(
                    "A box is written as longitudes, then latitudes: 16.18...16.58, 48.11...48.33",
                    "Eine Box schreibt man als Längen, dann Breiten: 16.18...16.58, 48.11...48.33",
                ),
            );
            return Err(());
        }
        Ok(Expr::new(ExprKind::BoxLit([a.min(b), a.max(b), c.min(d), c.max(d)]), sp))
    }

    /// `8.8079E, 53.07N` (or latitude first): one coordinate of each axis.
    fn point(&mut self) -> PResult<Expr> {
        let start = self.span();
        let Tok::Coord(a, ax) = self.bump().tok else { unreachable!() };
        let (b, bx) = match (self.tok().clone(), self.tok_at(1).clone()) {
            (Tok::Comma, Tok::Coord(b, bx)) => {
                self.p += 2;
                (b, bx)
            }
            _ => {
                self.diags.push(
                    Diagnostic::error("E108", start, Text2::new("A point needs a longitude and a latitude.", "Ein Punkt braucht eine Länge und eine Breite."))
                        .why("Write the longitude with E or W and the latitude with N or S:", "Schreibe die Länge mit E oder W und die Breite mit N oder S:")
                        .sugg("8.8079E, 53.0758N"),
                );
                return Err(());
            }
        };
        let sp = start.to(self.prev_span());
        let (lon, lat) = match (ax, bx) {
            ('E', 'N') => (a, b),
            ('N', 'E') => (b, a),
            _ => {
                self.diags.push(
                    Diagnostic::error("E108", sp, Text2::new("A point needs one longitude and one latitude.", "Ein Punkt braucht eine Länge und eine Breite."))
                        .why("One coordinate ends with E or W (longitude), the other with N or S (latitude):", "Eine Koordinate endet auf E oder W (Länge), die andere auf N oder S (Breite):")
                        .sugg("8.8079E, 53.0758N"),
                );
                return Err(());
            }
        };
        if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
            self.diags.push(Diagnostic::error("E108", sp, Text2::new("This point lies outside the world.", "Dieser Punkt liegt außerhalb der Welt.")));
            return Err(());
        }
        Ok(Expr::new(ExprKind::Point([lon, lat]), sp))
    }

    fn region(&mut self) -> PResult<Expr> {
        self.unary()
    }

    fn date_expr(&mut self) -> PResult<Expr> {
        let l = self.additive()?;
        if self.is_kw("before") || self.is_kw("after") {
            return self.rel_tail(l);
        }
        Ok(l)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.t().clone();
        let sp = t.span;
        match &t.tok {
            Tok::Num(v) => {
                let v = *v;
                if *self.tok_at(1) == Tok::Ellipsis {
                    return self.box_lit(None);
                }
                // 7 Mar 2020
                if let Tok::Ident(m) = self.tok_at(1).clone()
                    && let Some(mo) = dates::month_name(&m)
                    && let Tok::Num(y) = *self.tok_at(2)
                {
                    self.p += 3;
                    let end = self.prev_span();
                    return match dates::day(y as i64, mo, v as u32) {
                        Some(d) if v.fract() == 0.0 => Ok(Expr::new(ExprKind::Date(d), sp.to(end))),
                        _ => {
                            self.bad_date(sp.to(end));
                            Err(())
                        }
                    };
                }
                self.bump();
                Ok(Expr::new(ExprKind::Num(v), sp))
            }
            Tok::Coord(..) => self.point(),
            Tok::Date(s) => {
                self.bump();
                match dates::parse_iso(s) {
                    Some(d) => Ok(Expr::new(ExprKind::Date(d), sp)),
                    None => {
                        self.bad_date(sp);
                        Err(())
                    }
                }
            }
            Tok::BadDate(s) => {
                self.bump();
                self.e301(s, sp);
                Err(())
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::new(ExprKind::Str(s.clone()), sp))
            }
            Tok::LParen => {
                self.bump();
                let save = (self.inner_of, self.in_cond);
                self.inner_of = false;
                self.in_cond = false;
                let e = self.expr();
                (self.inner_of, self.in_cond) = save;
                let mut e = e?;
                let end = self.close(Tok::RParen, sp)?;
                e.span = sp.to(end);
                Ok(e)
            }
            Tok::LBracket => {
                self.bump();
                let save = (self.inner_of, self.in_cond);
                self.inner_of = false;
                self.in_cond = false;
                let mut xs = Vec::new();
                let r: PResult<()> = (|| {
                    if *self.tok() != Tok::RBracket {
                        xs.push(self.expr()?);
                        while *self.tok() == Tok::Comma {
                            self.bump();
                            xs.push(self.expr()?);
                        }
                    }
                    Ok(())
                })();
                (self.inner_of, self.in_cond) = save;
                r?;
                let end = self.close(Tok::RBracket, sp)?;
                Ok(Expr::new(ExprKind::List(xs), sp.to(end)))
            }
            Tok::Star => {
                if matches!(self.tok_at(1), Tok::Ident(_)) {
                    return self.selection(None);
                }
                self.unexpected("a value", "ein Wert");
                Err(())
            }
            Tok::Ident(w) => self.word(w.clone(), sp),
            _ => {
                self.unexpected("a value", "ein Wert");
                Err(())
            }
        }
    }

    fn bad_date(&mut self, sp: Span) {
        self.diags.push(
            Diagnostic::error("E108", sp, Text2::new("This date does not exist.", "Dieses Datum gibt es nicht."))
                .why("Dates are written as 2020, 2020-03, 2020-03-07, Mar 2020 or 7 Mar 2020.", "Daten schreibt man als 2020, 2020-03, 2020-03-07, Mar 2020 oder 7 Mar 2020."),
        );
    }

    fn e301(&mut self, s: &str, sp: Span) {
        let parts: Vec<u32> = s.split('/').filter_map(|p| p.parse().ok()).collect();
        let (a, b, mut y) = (parts[0], parts[1], parts[2] as i64);
        if y < 100 {
            y += 2000;
        }
        let mut d = Diagnostic::error(
            "E301",
            sp,
            Text2::new(format!("\"{s}\" can be read in two ways."), format!("\"{s}\" lässt sich auf zwei Arten lesen.")),
        );
        let r1 = (1..=12).contains(&b).then_some((a, b));
        let r2 = (1..=12).contains(&a).then_some((b, a));
        let desc = |(dd, mm): (u32, u32)| dates::format_long(y, mm, dd);
        match (r1, r2) {
            (Some(x), Some(z)) if x != z => {
                d = d.why(
                    format!("This could be {} or {}.", desc(x), desc(z)),
                    format!("Das kann der {} oder der {} sein.", desc(x), desc(z)),
                );
            }
            _ => {}
        }
        d = d.header(Header::PleaseWrite);
        for (dd, mm) in [r1, r2].into_iter().flatten() {
            if dates::day(y, mm, dd).is_some() {
                let long = desc((dd, mm));
                d.suggestions.push(crate::diag::Sugg {
                    label: None,
                    code: format!("{long:<15} or      {y}-{mm:02}-{dd:02}"),
                    note: Some(Text2::new(format!("for {dd} {}", month_full(mm)), format!("für den {dd}. {}", monat(mm)))),
                });
            }
        }
        d.suggestions.dedup();
        self.diags.push(d);
    }

    fn word(&mut self, w: String, sp: Span) -> PResult<Expr> {
        match w.as_str() {
            "none" => {
                self.bump();
                return Ok(Expr::new(ExprKind::None, sp));
            }
            "true" | "false" => {
                self.bump();
                return Ok(Expr::new(ExprKind::Bool(w == "true"), sp));
            }
            "_" => {
                self.bump();
                return Ok(Expr::new(ExprKind::Underscore, sp));
            }
            "pick" => return self.pick(),
            "the" | "all" | "some" | "groups" => {
                if let Some(e) = self.try_quantified(&w)? {
                    return Ok(e);
                }
            }
            "path" if matches!(self.tok_at(1), Tok::Ident(_)) && self.is_kw_at(2, "from") => return self.path(),
            "isochrone" if matches!(self.tok_at(1), Tok::Ident(_)) && self.is_kw_at(2, "from") => return self.isochrone(),
            "geocode" | "geocoded" => {
                self.bump();
                let query = self.primary()?;
                let sp2 = sp.to(query.span);
                return Ok(Expr::new(ExprKind::Special(Box::new(Special::Geocode { one: w == "geocoded", query: Box::new(query) })), sp2));
            }
            "reverse-geocode" | "reverse-geocoded" => {
                self.bump();
                let at = self.primary()?;
                let sp2 = sp.to(at.span);
                return Ok(Expr::new(ExprKind::Special(Box::new(Special::ReverseGeocode { one: w == "reverse-geocoded", at: Box::new(at) })), sp2));
            }
            "dataset" => {
                self.bump();
                if let Tok::Str(f) = self.tok().clone() {
                    let end = self.bump().span;
                    return Ok(Expr::new(ExprKind::Special(Box::new(Special::Dataset(f))), sp.to(end)));
                }
                self.unexpected("a file name in quotes", "ein Dateiname in Anführungszeichen");
                return Err(());
            }
            "split" => {
                self.bump();
                let set = self.unary()?;
                self.expect_kw("by")?;
                let by = self.unary()?;
                self.expect_kw("at")?;
                let at = self.expr()?;
                let sp2 = sp.to(at.span);
                return Ok(Expr::new(ExprKind::Special(Box::new(Special::Split { set: Box::new(set), by: Box::new(by), at: Box::new(at) })), sp2));
            }
            "correlate" | "regress" if !matches!(self.tok_at(1), Tok::Assign | Tok::Newline | Tok::Eof | Tok::Comma | Tok::RParen) => {
                self.bump();
                let a = self.additive()?;
                self.expect_kw(if w == "correlate" { "with" } else { "on" })?;
                let b = self.additive()?;
                let sp2 = sp.to(b.span);
                let s = if w == "correlate" { Special::Correlate(Box::new(a), Box::new(b)) } else { Special::Regress(Box::new(a), Box::new(b)) };
                return Ok(Expr::new(ExprKind::Special(Box::new(s)), sp2));
            }
            "confusion" if self.is_kw_at(1, "of") => {
                self.bump();
                self.bump();
                let a = self.additive()?;
                self.expect_kw("with")?;
                let b = self.additive()?;
                let sp2 = sp.to(b.span);
                return Ok(Expr::new(ExprKind::Special(Box::new(Special::Confusion(Box::new(a), Box::new(b)))), sp2));
            }
            "fit" if self.is_kw_at(1, "powerlaw") => {
                self.bump();
                self.bump();
                self.expect_kw("to")?;
                let a = self.additive()?;
                let sp2 = sp.to(a.span);
                return Ok(Expr::new(ExprKind::Special(Box::new(Special::FitPowerlaw(Box::new(a)))), sp2));
            }
            "forecast" if matches!(self.tok_at(1), Tok::Num(_)) => {
                self.bump();
                let Tok::Num(n) = self.bump().tok else { unreachable!() };
                let (u, us) = self.ident()?;
                let Some(unit) = TimeUnit::parse(&u) else {
                    self.diags.push(Diagnostic::error(
                        "E108",
                        us,
                        Text2::new("\"forecast\" counts in days, weeks, months or years.", "\"forecast\" zählt in days, weeks, months oder years."),
                    ));
                    return Err(());
                };
                self.expect_kw("of")?;
                let of = self.unary()?;
                let sp2 = sp.to(of.span);
                return Ok(Expr::new(ExprKind::Special(Box::new(Special::Forecast { n: n as u64, unit, of: Box::new(of) })), sp2));
            }
            "classify" if !matches!(self.tok_at(1), Tok::Assign | Tok::Newline | Tok::Eof | Tok::Comma | Tok::RParen) => {
                self.bump();
                let set = self.unary()?;
                self.expect_kw("by")?;
                let label = self.expr_no_using()?;
                self.expect_kw("using")?;
                let features = self.expr()?;
                let sp2 = sp.to(features.span);
                return Ok(Expr::new(
                    ExprKind::Special(Box::new(Special::Classify { set: Box::new(set), label: Box::new(label), features: Box::new(features) })),
                    sp2,
                ));
            }
            _ => {}
        }
        // `first version of X`, `last version of X`
        if (w == "first" || w == "last") && self.is_kw_at(1, "version") && self.is_kw_at(2, "of") {
            self.p += 1;
            let f = if w == "first" { OfFunc::FirstVersion } else { OfFunc::LastVersion };
            let fsp = sp.to(self.span());
            return self.of_call_from(f, fsp, sp);
        }
        // `nearest * x to a with …`: the same as `nearest of (* x with …) to a`
        if w == "nearest" && (*self.tok_at(1) == Tok::Star || matches!(self.tok_at(1), Tok::Ident(k) if Kind::parse(k).is_some())) {
            return self.nearest_short(sp);
        }
        if let Some(f) = OfFunc::parse(&w)
            && self.is_kw_at(1, "of")
        {
            return self.of_call(f, sp);
        }
        // `changesets from … to …`: selection without a variable
        if let Some((kind, true)) = Kind::parse(&w)
            && (self.is_kw_at(1, "within") || self.is_kw_at(1, "on") || self.is_kw_at(1, "from"))
        {
            self.bump();
            let pattern = vec![PatternItem { kind, plural: true, var: "_".into(), var_span: sp, kind_span: sp }];
            let mut c = Clauses::default();
            self.clauses(&mut c, false)?;
            let sel = Selection {
                source: None,
                pick: None,
                pattern,
                seq_threshold: None,
                randomly: false,
                seed: None,
                var_pred: None,
                cond: None,
                within: c.within,
                time: c.time,
                order: c.order,
            };
            let end = self.prev_span();
            return Ok(Expr::new(ExprKind::Select(Box::new(sel)), sp.to(end)));
        }
        // selections: `area A with …`
        if let Some((_, _)) = Kind::parse(&w)
            && let Tok::Ident(v) = self.tok_at(1)
            && !KEYWORDS.contains(&v.as_str())
        {
            return self.selection(None);
        }
        // `homes h with …`: filter over a named set
        if let Tok::Ident(v) = self.tok_at(1).clone()
            && !KEYWORDS.contains(&v.as_str())
            && !KEYWORDS.contains(&w.as_str())
            && Kind::parse(&w).is_none()
            && units::unit(&v).is_none_or(|_| self.is_kw_at(2, "with"))
        {
            self.bump();
            let src = Expr::new(ExprKind::Ident(w), sp);
            let (var, var_span) = self.ident()?;
            let pattern = vec![PatternItem { kind: Kind::Any, plural: true, var, var_span, kind_span: var_span }];
            let mut e = self.selection_rest(None, sp, pattern)?;
            let ExprKind::Select(sel) = &mut e.kind else { unreachable!() };
            sel.source = Some(Box::new(src));
            e.span = sp.to(e.span);
            return Ok(e);
        }
        // `Mar 2020`
        if let Some(m) = dates::month_name(&w)
            && let Tok::Num(y) = *self.tok_at(1)
            && y.fract() == 0.0
            && (1000.0..10000.0).contains(&y)
        {
            self.p += 2;
            return Ok(Expr::new(ExprKind::Date(dates::month(y as i64, m).unwrap()), sp.to(self.prev_span())));
        }
        self.bump();
        Ok(Expr::new(ExprKind::Ident(w), sp))
    }

    /// Lambda body for `classify … by x => LABEL using …`: `using` ends it.
    fn expr_no_using(&mut self) -> PResult<Expr> {
        self.expr()
    }

    fn pick(&mut self) -> PResult<Expr> {
        let s = self.bump().span;
        let exactly = self.eat_kw("exactly");
        let n = if let Tok::Num(v) = *self.tok() {
            self.bump();
            if v.fract() != 0.0 || v < 0.0 {
                self.diags.push(Diagnostic::error(
                    "E108",
                    self.prev_span(),
                    Text2::new("\"pick\" needs a whole number.", "\"pick\" braucht eine ganze Zahl."),
                ));
                return Err(());
            }
            Some(v as u64)
        } else {
            None
        };
        let pick = Pick { exactly, n, span: s.to(self.prev_span()) };
        // `pick k in SET …`
        if let Tok::Ident(v) = self.tok().clone()
            && self.is_kw_at(1, "in")
            && Kind::parse(&v).is_none()
        {
            let (var, var_span) = self.ident()?;
            self.bump();
            return self.quantified_rest(QKind::Pick { n, exactly }, None, var, var_span, s);
        }
        self.selection(Some(pick))
    }

    fn selection(&mut self, pick: Option<Pick>) -> PResult<Expr> {
        let start = pick.as_ref().map_or(self.span(), |p| p.span);
        let mut pattern = Vec::new();
        while pattern.is_empty() || *self.tok() == Tok::Arrow {
            if !pattern.is_empty() {
                self.bump();
            }
            let ks = self.span();
            let (kind, plural) = if *self.tok() == Tok::Star {
                self.bump();
                // `*` has no plural form: `pick * x` is one element, otherwise a set
                (Kind::Any, !matches!(&pick, Some(p) if p.n.is_none()))
            } else {
                let (k, sp) = self.ident()?;
                match Kind::parse(&k) {
                    Some(x) => x,
                    None => {
                        let mut d = Diagnostic::error(
                            "E108",
                            sp,
                            Text2::new(format!("\"{k}\" is not a kind of object."), format!("\"{k}\" ist keine Objektsorte.")),
                        )
                        .why(
                            "Possible are node, way, area, relation, *, changeset, user, note and key (also in plural).",
                            "Möglich sind node, way, area, relation, *, changeset, user, note und key (auch in der Mehrzahl).",
                        );
                        if let Some(c) = crate::diag::closest(&k, ["node", "nodes", "way", "ways", "area", "areas", "relation", "relations"]) {
                            d = d.header(Header::DidYouMean).sugg(c.to_string());
                        }
                        self.diags.push(d);
                        return Err(());
                    }
                }
            };
            let (var, var_span) = self.ident()?;
            if KEYWORDS.contains(&var.as_str()) && var != "_" {
                self.p -= 1;
                self.unexpected("a variable name", "ein Variablenname");
                return Err(());
            }
            pattern.push(PatternItem { kind, plural, var, var_span, kind_span: ks });
        }
        self.selection_rest(pick, start, pattern)
    }

    fn selection_rest(&mut self, pick: Option<Pick>, start: Span, pattern: Vec<PatternItem>) -> PResult<Expr> {
        let seq_threshold = if pattern.len() > 1 && self.is_kw("closer") { Some(self.threshold()?) } else { None };
        let mut randomly = false;
        let mut seed = None;
        if self.eat_kw("randomly") {
            randomly = true;
        }
        if self.is_kw("using") && self.is_kw_at(1, "seed") {
            let us = self.bump().span;
            self.bump();
            let n = match *self.tok() {
                Tok::Num(v) => {
                    self.bump();
                    v as u64
                }
                _ => {
                    self.unexpected("a number", "eine Zahl");
                    return Err(());
                }
            };
            if !randomly {
                let usp = us.to(self.prev_span());
                let line_end = self.line_end();
                let line_start = self.stmt_start;
                let txt = &self.src[line_start..line_end];
                let rel = us.start as usize - line_start;
                let fixed = format!("{}randomly {}", &txt[..rel], &txt[rel..]);
                self.diags.push(
                    Diagnostic::error(
                        "E106",
                        usp,
                        Text2::new("\"using seed\" only makes sense together with \"randomly\".", "\"using seed\" ergibt nur zusammen mit \"randomly\" Sinn."),
                    )
                    .header(Header::DidYouMean)
                    .sugg(fixed.trim().to_string()),
                );
                return Err(());
            }
            seed = Some(n);
        }
        // `* b near some _ in …` before `with`
        let mut var_pred = None;
        if self.at_rel_tail() && !self.is_kw("within") && *self.tok() != Tok::Assign {
            let last = pattern.last().unwrap();
            let v = Expr::new(ExprKind::Ident(last.var.clone()), last.var_span);
            let save = self.in_cond;
            self.in_cond = true;
            let r = self.rel_tail(v);
            self.in_cond = save;
            var_pred = Some(Box::new(r?));
        }
        let cond = if self.eat_kw("with") {
            let save = self.in_cond;
            self.in_cond = true;
            let r = self.expr();
            self.in_cond = save;
            Some(Box::new(r?))
        } else {
            None
        };
        let mut sel = Selection {
            source: None,
            pick,
            pattern,
            seq_threshold,
            randomly,
            seed,
            var_pred,
            cond,
            within: None,
            time: None,
            order: None,
        };
        let mut c = Clauses::default();
        self.clauses(&mut c, false)?;
        sel.within = c.within;
        sel.time = c.time;
        sel.order = c.order;
        let end = self.prev_span();
        Ok(Expr::new(ExprKind::Select(Box::new(sel)), start.to(end)))
    }

    fn try_quantified(&mut self, w: &str) -> PResult<Option<Expr>> {
        // the [kind] x in …
        let (kind, var_k) = match self.tok_at(1) {
            Tok::Ident(k) if Kind::parse(k).is_some() && matches!(self.tok_at(2), Tok::Ident(_)) && self.is_kw_at(3, "in") => {
                (Kind::parse(k).map(|x| x.0), 2)
            }
            Tok::Ident(_) if self.is_kw_at(2, "in") => (None, 1),
            _ => return Ok(None),
        };
        let q = match w {
            "the" => QKind::The,
            "all" => QKind::All,
            "some" => QKind::Some,
            _ => QKind::Groups,
        };
        let s = self.bump().span;
        self.p += var_k - 1;
        let (var, var_span) = self.ident()?;
        self.expect_kw("in")?;
        self.quantified_rest(q, kind, var, var_span, s).map(Some)
    }

    fn quantified_rest(&mut self, q: QKind, kind: Option<Kind>, var: String, var_span: Span, start: Span) -> PResult<Expr> {
        let set = self.unary()?;
        let pred = if self.at_rel_tail() && !self.is_kw("in") {
            let v = Expr::new(ExprKind::Ident(var.clone()), var_span);
            let save = self.in_cond;
            self.in_cond = true;
            let r = self.rel_tail(v);
            self.in_cond = save;
            Some(Box::new(r?))
        } else {
            None
        };
        let cond = if self.eat_kw("with") {
            let save = self.in_cond;
            self.in_cond = true;
            let r = self.expr();
            self.in_cond = save;
            Some(Box::new(r?))
        } else {
            None
        };
        let end = self.prev_span();
        Ok(Expr::new(ExprKind::Quantified { q, kind, var, var_span, set: Box::new(set), pred, cond }, start.to(end)))
    }

    /// `nearest KIND VAR [to X] [with …] [within …] [to X] [by MODE]`, read as
    /// `nearest of (KIND VAR with … within …) to X by MODE`.
    fn nearest_short(&mut self, sp: Span) -> PResult<Expr> {
        self.bump(); // nearest
        let ks = self.span();
        let kind = if *self.tok() == Tok::Star {
            self.bump();
            Kind::Any
        } else {
            let (k, _) = self.ident()?;
            Kind::parse(&k).map(|x| x.0).unwrap_or(Kind::Any)
        };
        let (var, var_span) = self.ident()?;
        if KEYWORDS.contains(&var.as_str()) {
            self.p -= 1;
            self.unexpected("a variable name", "ein Variablenname");
            return Err(());
        }
        let mut to = None;
        if self.eat_kw("to") {
            to = Some(Box::new(self.unary()?));
        }
        let pattern = vec![PatternItem { kind, plural: true, var, var_span, kind_span: ks }];
        let sel = self.selection_rest(None, ks, pattern)?;
        if to.is_none() && self.eat_kw("to") {
            to = Some(Box::new(self.unary()?));
        }
        let mut clauses = Clauses { to, ..Default::default() };
        if self.is_kw("by") && matches!(self.tok_at(1), Tok::Ident(m) if Mode::parse(m).is_some()) {
            clauses.mode = Some(self.mode_clause()?);
        }
        let end = self.prev_span();
        Ok(Expr::new(ExprKind::Of { func: OfFunc::Nearest, func_span: sp, operand: OfOperand::Expr(Box::new(sel)), clauses: Box::new(clauses) }, sp.to(end)))
    }

    fn of_call(&mut self, func: OfFunc, fsp: Span) -> PResult<Expr> {
        let start = self.span();
        self.of_call_from(func, fsp, start)
    }

    fn of_call_from(&mut self, func: OfFunc, fsp: Span, start: Span) -> PResult<Expr> {
        self.bump(); // function word
        self.expect_kw("of")?;
        let operand = if let Tok::Ident(v) = self.tok().clone()
            && self.is_kw_at(1, "in")
            && !KEYWORDS.contains(&v.as_str())
        {
            let var_span = self.bump().span;
            self.bump();
            let save = self.inner_of;
            self.inner_of = true;
            let set = self.unary();
            self.inner_of = save;
            let set = set?;
            let pred = if self.at_rel_tail() && !self.is_kw("in") && !self.is_kw("within") {
                let ve = Expr::new(ExprKind::Ident(v.clone()), var_span);
                let save = self.in_cond;
                self.in_cond = true;
                let r = self.rel_tail(ve);
                self.in_cond = save;
                Some(Box::new(r?))
            } else {
                None
            };
            let cond = if self.eat_kw("with") {
                let save = self.in_cond;
                self.in_cond = true;
                let r = self.expr();
                self.in_cond = save;
                Some(Box::new(r?))
            } else {
                None
            };
            OfOperand::Iter { var: v, var_span, set: Box::new(set), pred, cond }
        } else {
            let save = self.inner_of;
            self.inner_of = true;
            let e = self.unary();
            self.inner_of = save;
            OfOperand::Expr(Box::new(e?))
        };
        let mut clauses = Clauses::default();
        if !self.inner_of {
            self.clauses(&mut clauses, true)?;
        }
        let end = self.prev_span();
        Ok(Expr::new(ExprKind::Of { func, func_span: fsp, operand, clauses: Box::new(clauses) }, start.to(end)))
    }

    /// Clauses in fixed order (§3.5). `full` allows the aggregate-only clauses.
    fn clauses(&mut self, c: &mut Clauses, full: bool) -> PResult<()> {
        if full && self.is_kw("with") {
            self.bump();
            let save = self.in_cond;
            self.in_cond = true;
            let r = self.expr();
            self.in_cond = save;
            c.with = Some(Box::new(r?));
        }
        if self.eat_kw("within") {
            c.within = Some(Box::new(self.region()?));
        }
        if self.is_kw("on") {
            self.bump();
            if self.eat_kw("each") {
                let (u, us) = self.ident()?;
                let Some(unit) = TimeUnit::parse(&u).filter(|_| !u.ends_with('s')) else {
                    self.diags.push(
                        Diagnostic::error(
                            "E108",
                            us,
                            Text2::new("\"on each\" needs day, week, month or year.", "\"on each\" braucht day, week, month oder year."),
                        )
                        .header(Header::ForExample)
                        .sugg("on each month from 2010 to 2020"),
                    );
                    return Err(());
                };
                c.time = Some(TimeRef::OnEach { unit, span: us, range: None });
            } else {
                c.time = Some(TimeRef::On(Box::new(self.date_expr()?)));
            }
        }
        if full {
            while self.is_kw("per") {
                let ps = self.bump().span;
                let g = self.grouping()?;
                c.per.push((g, ps.to(self.prev_span())));
            }
            if self.eat_kw("having") {
                let save = self.in_cond;
                self.in_cond = true;
                let r = self.expr();
                self.in_cond = save;
                c.having = Some(Box::new(r?));
            }
        }
        if self.is_kw("ordered") && self.is_kw_at(1, "by") {
            self.bump();
            self.bump();
            c.order = Some(Box::new(self.unary()?));
        }
        if full {
            if self.is_kw("to") {
                self.bump();
                c.to = Some(Box::new(self.unary()?));
            }
            if self.is_kw("by") && matches!(self.tok_at(1), Tok::Ident(m) if Mode::parse(m).is_some()) {
                c.mode = Some(self.mode_clause()?);
            }
        }
        if self.is_kw("from") {
            let fs = self.bump().span;
            let a = self.date_expr()?;
            self.expect_kw("to")?;
            let b = self.date_expr()?;
            let fe = self.prev_span();
            match &mut c.time {
                Some(TimeRef::OnEach { range, .. }) => *range = Some((Box::new(a), Box::new(b))),
                Some(TimeRef::On(_)) => {
                    self.diags.push(
                        Diagnostic::error(
                            "E108",
                            fs.to(fe),
                            Text2::new("\"on\" and \"from … to\" cannot be used together.", "\"on\" und \"from … to\" gehen nicht zusammen."),
                        )
                        .why(
                            "\"on\" asks for the state at one moment, \"from … to\" for the changes in a period.\nFor the state at the start of every month, write \"on each month from … to …\".",
                            "\"on\" fragt nach dem Zustand zu einem Zeitpunkt, \"from … to\" nach den Änderungen in einem Zeitraum.\nFür den Zustand zu Beginn jedes Monats schreibt man \"on each month from … to …\".",
                        ),
                    );
                    return Err(());
                }
                None => c.time = Some(TimeRef::Range(Box::new(a), Box::new(b))),
                Some(TimeRef::Range(..)) => unreachable!(),
            }
            // E104: something follows the time range
            if let Some(rank) = self.clause_rank_here(full) {
                let fixed = self.reorder_fix(fs, fe, rank);
                self.diags.push(
                    Diagnostic::error(
                        "E104",
                        fs.to(fe),
                        Text2::new("\"from … to\" has to come last.", "\"from … to\" muss am Ende stehen."),
                    )
                    .header(Header::DidYouMean)
                    .sugg(fixed),
                );
                return Err(());
            }
        }
        if let Some(TimeRef::OnEach { unit, span, range: None }) = &c.time {
            let u = unit.name();
            self.diags.push(
                Diagnostic::error(
                    "E110",
                    *span,
                    Text2::new(format!("\"on each {u}\" needs a time range."), format!("\"on each {u}\" braucht einen Zeitraum.")),
                )
                .why("Add \"from … to …\" at the end, for example:", "Ergänze am Ende \"from … to …\", zum Beispiel:")
                .sugg(format!("on each {u} from 2010 to 2020")),
            );
            return Err(());
        }
        Ok(())
    }

    /// Rank of a clause keyword (order of §3.5) at token offset `k`.
    fn clause_rank_at(&self, k: usize, full: bool) -> Option<u8> {
        let Tok::Ident(w) = self.tok_at(k) else { return None };
        Some(match w.as_str() {
            "with" if full => 0,
            "within" => 1,
            "on" => 2,
            "per" if full => 3,
            "having" if full => 4,
            "ordered" if self.is_kw_at(k + 1, "by") => 5,
            _ => return None,
        })
    }

    fn clause_rank_here(&self, full: bool) -> Option<u8> {
        self.clause_rank_at(0, full)
    }

    /// Suggestion for E104: moves the misplaced clauses before the first clause of a higher
    /// rank and puts `from … to` at the end.
    fn reorder_fix(&self, fs: Span, fe: Span, rank: u8) -> String {
        let line_end = self.line_end();
        let range_txt = self.text(fs.to(fe)).to_string();
        let rest = self.src[fe.end as usize..line_end].trim().to_string();
        // find the insertion point among the clauses before `from`
        let mut k = self.p;
        while k > 0 && self.toks[k - 1].span.start as usize >= self.stmt_start {
            k -= 1;
        }
        let mut insert_at = fs.start as usize;
        let mut depth = 0i32;
        let first = k;
        for i in first..self.toks.len() {
            let t = &self.toks[i];
            if t.span.start >= fs.start {
                break;
            }
            match t.tok {
                Tok::LParen | Tok::LBracket => depth += 1,
                Tok::RParen | Tok::RBracket => depth -= 1,
                _ => {}
            }
            let rank_here = match &t.tok {
                Tok::Ident(w) => match w.as_str() {
                    "within" => Some(1),
                    "on" => Some(2),
                    "per" => Some(3),
                    "having" => Some(4),
                    _ => None,
                },
                _ => None,
            };
            if depth == 0
                && let Some(r) = rank_here
                && r > rank
            {
                insert_at = t.span.start as usize;
                break;
            }
        }
        let head = self.src[self.stmt_start..insert_at].trim_end();
        let mid = self.src[insert_at..fs.start as usize].trim();
        let mut out = format!("{head} {rest}");
        if !mid.is_empty() {
            out.push(' ');
            out.push_str(mid);
        }
        out.push(' ');
        out.push_str(&range_txt);
        out
    }

    fn grouping(&mut self) -> PResult<Grouping> {
        if self.is_kw("cell") && self.is_kw_at(1, "of") {
            self.bump();
            self.bump();
            return Ok(Grouping::Cell(Box::new(self.unary()?)));
        }
        if self.is_kw("tile") && self.is_kw_at(1, "at") {
            self.bump();
            self.bump();
            self.expect_kw("zoom")?;
            let Tok::Num(z) = *self.tok() else {
                self.unexpected("a zoom level", "eine Zoomstufe");
                return Err(());
            };
            self.bump();
            return Ok(Grouping::Tile(z as u32));
        }
        if self.is_kw("chunk") && self.is_kw_at(1, "of") {
            self.bump();
            self.bump();
            let Tok::Num(n) = *self.tok() else {
                self.unexpected("a number", "eine Zahl");
                return Err(());
            };
            self.bump();
            return Ok(Grouping::Chunk(n as u64));
        }
        if let Tok::Ident(w) = self.tok().clone()
            && let Some(u) = TimeUnit::parse(&w)
            && !w.ends_with('s')
            && !matches!(self.tok_at(1), Tok::Dot | Tok::Colon)
        {
            self.bump();
            return Ok(Grouping::Time(u));
        }
        Ok(Grouping::Expr(Box::new(self.unary()?)))
    }

    fn path(&mut self) -> PResult<Expr> {
        let s = self.bump().span;
        let (var, _) = self.ident()?;
        self.expect_kw("from")?;
        let from = self.unary()?;
        self.expect_kw("to")?;
        let to = self.unary()?;
        let mode = self.mode_clause()?;
        let sp = s.to(self.prev_span());
        Ok(Expr::new(ExprKind::Special(Box::new(Special::Path { var, from: Box::new(from), to: Box::new(to), mode })), sp))
    }

    fn isochrone(&mut self) -> PResult<Expr> {
        let s = self.bump().span;
        let (var, _) = self.ident()?;
        self.expect_kw("from")?;
        let from = self.unary()?;
        let threshold = self.threshold()?;
        let sp = s.to(self.prev_span());
        Ok(Expr::new(ExprKind::Special(Box::new(Special::Isochrone { var, from: Box::new(from), threshold })), sp))
    }
}

fn month_full(m: u32) -> &'static str {
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"]
        [(m - 1) as usize]
}

fn monat(m: u32) -> &'static str {
    ["Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"]
        [(m - 1) as usize]
}

