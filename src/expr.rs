//! Row expression language (SPEC §10.2) for subset `where` filters, the
//! `conditional` generator, and the `algebraic` generator.
//!
//! Grammar (Pratt parser, lowest to highest precedence):
//!   or → and → not → comparison (== != < <= > >= like, in, is null) →
//!   additive (+ -) → multiplicative (* / %) → unary minus → primary
//! Primary: number, 'string' or "string", true/false/null, column ref, (expr).
//!
//! Null semantics are SQL-flavored: comparisons and arithmetic involving NULL
//! yield NULL (which is falsy); `is null` / `is not null` test explicitly;
//! and/or use three-valued logic collapsed to false at the top level.

use crate::value::Value;
use crate::{verr, Result};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(f64),
    Int(i64),
    Str(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Column(String),
    LitInt(i64),
    LitFloat(f64),
    LitStr(String),
    LitBool(bool),
    LitNull,
    Unary { op: UnOp, expr: Box<Expr> },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    In { expr: Box<Expr>, items: Vec<Expr>, negated: bool },
    IsNull { expr: Box<Expr>, negated: bool },
    Like { expr: Box<Expr>, pattern: String, negated: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Tri-state boolean for SQL-ish null propagation.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tri {
    True,
    False,
    Unknown,
}

impl Expr {
    pub fn parse(input: &str) -> Result<Expr> {
        let toks = lex(input)?;
        let mut p = EParser { toks, pos: 0 };
        let e = p.or_expr()?;
        if p.pos != p.toks.len() {
            return Err(verr(format!("expression: unexpected trailing tokens in {input:?}")));
        }
        Ok(e)
    }

    /// All column names referenced (for validation).
    pub fn columns(&self, out: &mut Vec<String>) {
        match self {
            Expr::Column(c) => out.push(c.clone()),
            Expr::Unary { expr, .. } => expr.columns(out),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.columns(out);
                rhs.columns(out);
            }
            Expr::In { expr, items, .. } => {
                expr.columns(out);
                for i in items {
                    i.columns(out);
                }
            }
            Expr::IsNull { expr, .. } | Expr::Like { expr, .. } => expr.columns(out),
            _ => {}
        }
    }

    /// Evaluate to a Value. `lookup` resolves column names.
    pub fn eval(&self, lookup: &dyn Fn(&str) -> Option<Value>) -> Result<Value> {
        Ok(match self {
            Expr::Column(c) => {
                lookup(c).ok_or_else(|| verr(format!("expression references unknown column {c:?}")))?
            }
            Expr::LitInt(i) => Value::Int(*i),
            Expr::LitFloat(f) => Value::Float(*f),
            Expr::LitStr(s) => Value::Text(s.clone()),
            Expr::LitBool(b) => Value::Bool(*b),
            Expr::LitNull => Value::Null,
            Expr::Unary { op: UnOp::Neg, expr } => match expr.eval(lookup)? {
                Value::Int(i) => Value::Int(-i),
                Value::Float(f) => Value::Float(-f),
                Value::Null => Value::Null,
                v => return Err(verr(format!("cannot negate {v:?}"))),
            },
            Expr::Unary { op: UnOp::Not, expr } => match truth_of(&expr.eval(lookup)?) {
                Tri::True => Value::Bool(false),
                Tri::False => Value::Bool(true),
                Tri::Unknown => Value::Null,
            },
            Expr::Binary { op, lhs, rhs } => eval_binary(*op, lhs, rhs, lookup)?,
            Expr::In { expr, items, negated } => {
                let v = expr.eval(lookup)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let mut found = false;
                for item in items {
                    let iv = item.eval(lookup)?;
                    if compare_eq(&v, &iv) == Tri::True {
                        found = true;
                        break;
                    }
                }
                Value::Bool(found != *negated)
            }
            Expr::IsNull { expr, negated } => {
                let v = expr.eval(lookup)?;
                Value::Bool(v.is_null() != *negated)
            }
            Expr::Like { expr, pattern, negated } => {
                let v = expr.eval(lookup)?;
                match v {
                    Value::Null => Value::Null,
                    Value::Text(s) => Value::Bool(like_match(pattern, &s) != *negated),
                    other => Value::Bool(like_match(pattern, &other.to_string()) != *negated),
                }
            }
        })
    }

    /// Evaluate as a predicate: NULL/unknown collapses to false.
    pub fn eval_bool(&self, lookup: &dyn Fn(&str) -> Option<Value>) -> Result<bool> {
        Ok(truth_of(&self.eval(lookup)?) == Tri::True)
    }
}

fn truth_of(v: &Value) -> Tri {
    match v {
        Value::Null => Tri::Unknown,
        Value::Bool(true) => Tri::True,
        Value::Bool(false) => Tri::False,
        Value::Int(i) => {
            if *i != 0 {
                Tri::True
            } else {
                Tri::False
            }
        }
        Value::Float(f) => {
            if *f != 0.0 {
                Tri::True
            } else {
                Tri::False
            }
        }
        Value::Text(_) => Tri::False,
    }
}

fn compare_eq(a: &Value, b: &Value) -> Tri {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Tri::Unknown,
        (Value::Bool(x), Value::Bool(y)) => tri(x == y),
        (Value::Text(x), Value::Text(y)) => tri(x == y),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => tri(x == y),
            _ => tri(false),
        },
    }
}

fn tri(b: bool) -> Tri {
    if b {
        Tri::True
    } else {
        Tri::False
    }
}

fn compare_ord(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => None,
        },
    }
}

fn eval_binary(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    lookup: &dyn Fn(&str) -> Option<Value>,
) -> Result<Value> {
    use std::cmp::Ordering::*;
    // Short-circuit logic ops with three-valued semantics.
    if matches!(op, BinOp::And | BinOp::Or) {
        let l = truth_of(&lhs.eval(lookup)?);
        return Ok(match (op, l) {
            (BinOp::And, Tri::False) => Value::Bool(false),
            (BinOp::Or, Tri::True) => Value::Bool(true),
            _ => {
                let r = truth_of(&rhs.eval(lookup)?);
                match (op, l, r) {
                    (BinOp::And, _, Tri::False) => Value::Bool(false),
                    (BinOp::And, Tri::True, Tri::True) => Value::Bool(true),
                    (BinOp::Or, _, Tri::True) => Value::Bool(true),
                    (BinOp::Or, Tri::False, Tri::False) => Value::Bool(false),
                    _ => Value::Null,
                }
            }
        });
    }
    let a = lhs.eval(lookup)?;
    let b = rhs.eval(lookup)?;
    Ok(match op {
        BinOp::Eq => match compare_eq(&a, &b) {
            Tri::True => Value::Bool(true),
            Tri::False => Value::Bool(false),
            Tri::Unknown => Value::Null,
        },
        BinOp::Ne => match compare_eq(&a, &b) {
            Tri::True => Value::Bool(false),
            Tri::False => Value::Bool(true),
            Tri::Unknown => Value::Null,
        },
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => match compare_ord(&a, &b) {
            None => Value::Null,
            Some(ord) => Value::Bool(matches!(
                (op, ord),
                (BinOp::Lt, Less)
                    | (BinOp::Le, Less)
                    | (BinOp::Le, Equal)
                    | (BinOp::Gt, Greater)
                    | (BinOp::Ge, Greater)
                    | (BinOp::Ge, Equal)
            )),
        },
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            if a.is_null() || b.is_null() {
                return Ok(Value::Null);
            }
            match (&a, &b) {
                (Value::Int(x), Value::Int(y)) => match op {
                    BinOp::Add => Value::Int(x.wrapping_add(*y)),
                    BinOp::Sub => Value::Int(x.wrapping_sub(*y)),
                    BinOp::Mul => Value::Int(x.wrapping_mul(*y)),
                    BinOp::Div => {
                        if *y == 0 {
                            Value::Null
                        } else {
                            Value::Int(x / y)
                        }
                    }
                    BinOp::Mod => {
                        if *y == 0 {
                            Value::Null
                        } else {
                            Value::Int(x % y)
                        }
                    }
                    _ => unreachable!(),
                },
                _ => {
                    let (x, y) = (
                        a.as_f64().ok_or_else(|| verr(format!("arithmetic on non-number {a:?}")))?,
                        b.as_f64().ok_or_else(|| verr(format!("arithmetic on non-number {b:?}")))?,
                    );
                    match op {
                        BinOp::Add => Value::Float(x + y),
                        BinOp::Sub => Value::Float(x - y),
                        BinOp::Mul => Value::Float(x * y),
                        BinOp::Div => {
                            if y == 0.0 {
                                Value::Null
                            } else {
                                Value::Float(x / y)
                            }
                        }
                        BinOp::Mod => {
                            if y == 0.0 {
                                Value::Null
                            } else {
                                Value::Float(x % y)
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        BinOp::And | BinOp::Or => unreachable!(),
    })
}

/// SQL LIKE: % matches any run, _ matches one char. Case-sensitive.
fn like_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Simple DP over (pattern, text) positions.
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '%' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                '%' => dp[i - 1][j] || dp[i][j - 1],
                '_' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == t[j - 1],
            };
        }
    }
    dp[p.len()][t.len()]
}

fn lex(input: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(verr("expression: unterminated string"));
                    }
                    if chars[i] == quote {
                        // Doubled quote escapes itself (SQL style).
                        if chars.get(i + 1) == Some(&quote) {
                            s.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                toks.push(Tok::Str(s));
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Op("=="));
                    i += 2;
                } else {
                    // Single = also accepted as equality (SQL habit).
                    toks.push(Tok::Op("=="));
                    i += 1;
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Op("!="));
                    i += 2;
                } else {
                    return Err(verr("expression: unexpected '!'"));
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Op("<="));
                    i += 2;
                } else if chars.get(i + 1) == Some(&'>') {
                    toks.push(Tok::Op("!="));
                    i += 2;
                } else {
                    toks.push(Tok::Op("<"));
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Op(">="));
                    i += 2;
                } else {
                    toks.push(Tok::Op(">"));
                    i += 1;
                }
            }
            '+' => {
                toks.push(Tok::Op("+"));
                i += 1;
            }
            '-' => {
                toks.push(Tok::Op("-"));
                i += 1;
            }
            '*' => {
                toks.push(Tok::Op("*"));
                i += 1;
            }
            '/' => {
                toks.push(Tok::Op("/"));
                i += 1;
            }
            '%' => {
                toks.push(Tok::Op("%"));
                i += 1;
            }
            c if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) => {
                let start = i;
                let mut is_float = false;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    if chars[i] == '.' {
                        is_float = true;
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if is_float {
                    toks.push(Tok::Num(
                        text.parse().map_err(|_| verr(format!("expression: bad number {text:?}")))?,
                    ));
                } else {
                    toks.push(Tok::Int(
                        text.parse().map_err(|_| verr(format!("expression: bad number {text:?}")))?,
                    ));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                    i += 1;
                }
                toks.push(Tok::Ident(chars[start..i].iter().collect()));
            }
            _ => return Err(verr(format!("expression: unexpected character {c:?}"))),
        }
    }
    Ok(toks)
}

struct EParser {
    toks: Vec<Tok>,
    pos: usize,
}

impl EParser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.peek_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if *o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.and_expr()?;
        while self.eat_kw("or") {
            let rhs = self.and_expr()?;
            lhs = Expr::Binary { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.not_expr()?;
        while self.eat_kw("and") {
            let rhs = self.not_expr()?;
            lhs = Expr::Binary { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            let e = self.not_expr()?;
            return Ok(Expr::Unary { op: UnOp::Not, expr: Box::new(e) });
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let lhs = self.additive()?;
        // is [not] null
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            if !self.eat_kw("null") {
                return Err(verr("expression: expected NULL after IS"));
            }
            return Ok(Expr::IsNull { expr: Box::new(lhs), negated });
        }
        // [not] in / [not] like
        let negated = self.eat_kw("not");
        if self.eat_kw("in") {
            if !matches!(self.peek(), Some(Tok::LParen)) {
                return Err(verr("expression: expected '(' after IN"));
            }
            self.pos += 1;
            let mut items = Vec::new();
            loop {
                items.push(self.additive()?);
                match self.peek() {
                    Some(Tok::Comma) => self.pos += 1,
                    Some(Tok::RParen) => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(verr("expression: expected ',' or ')' in IN list")),
                }
            }
            return Ok(Expr::In { expr: Box::new(lhs), items, negated });
        }
        if self.eat_kw("like") {
            let pattern = match self.peek() {
                Some(Tok::Str(s)) => s.clone(),
                _ => return Err(verr("expression: LIKE requires a string literal pattern")),
            };
            self.pos += 1;
            return Ok(Expr::Like { expr: Box::new(lhs), pattern, negated });
        }
        if negated {
            return Err(verr("expression: expected IN or LIKE after NOT"));
        }
        for (tok, op) in [
            ("==", BinOp::Eq),
            ("!=", BinOp::Ne),
            ("<=", BinOp::Le),
            (">=", BinOp::Ge),
            ("<", BinOp::Lt),
            (">", BinOp::Gt),
        ] {
            if self.eat_op(tok) {
                let rhs = self.additive()?;
                return Ok(Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) });
            }
        }
        Ok(lhs)
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut lhs = self.multiplicative()?;
        loop {
            if self.eat_op("+") {
                let rhs = self.multiplicative()?;
                lhs = Expr::Binary { op: BinOp::Add, lhs: Box::new(lhs), rhs: Box::new(rhs) };
            } else if self.eat_op("-") {
                let rhs = self.multiplicative()?;
                lhs = Expr::Binary { op: BinOp::Sub, lhs: Box::new(lhs), rhs: Box::new(rhs) };
            } else {
                return Ok(lhs);
            }
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.unary()?;
        loop {
            if self.eat_op("*") {
                let rhs = self.unary()?;
                lhs = Expr::Binary { op: BinOp::Mul, lhs: Box::new(lhs), rhs: Box::new(rhs) };
            } else if self.eat_op("/") {
                let rhs = self.unary()?;
                lhs = Expr::Binary { op: BinOp::Div, lhs: Box::new(lhs), rhs: Box::new(rhs) };
            } else if self.eat_op("%") {
                let rhs = self.unary()?;
                lhs = Expr::Binary { op: BinOp::Mod, lhs: Box::new(lhs), rhs: Box::new(rhs) };
            } else {
                return Ok(lhs);
            }
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_op("-") {
            let e = self.unary()?;
            return Ok(Expr::Unary { op: UnOp::Neg, expr: Box::new(e) });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().cloned() {
            Some(Tok::Int(i)) => {
                self.pos += 1;
                Ok(Expr::LitInt(i))
            }
            Some(Tok::Num(f)) => {
                self.pos += 1;
                Ok(Expr::LitFloat(f))
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Expr::LitStr(s))
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let e = self.or_expr()?;
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    return Err(verr("expression: missing ')'"));
                }
                self.pos += 1;
                Ok(e)
            }
            Some(Tok::Ident(id)) => {
                self.pos += 1;
                if id.eq_ignore_ascii_case("true") {
                    Ok(Expr::LitBool(true))
                } else if id.eq_ignore_ascii_case("false") {
                    Ok(Expr::LitBool(false))
                } else if id.eq_ignore_ascii_case("null") {
                    Ok(Expr::LitNull)
                } else {
                    Ok(Expr::Column(id))
                }
            }
            other => Err(verr(format!("expression: unexpected token {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(pairs: &'a [(&'a str, Value)]) -> impl Fn(&str) -> Option<Value> + 'a {
        move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn arithmetic_and_comparison() {
        let e = Expr::parse("quantity * unit_price > 100").unwrap();
        let row = [("quantity", Value::Int(3)), ("unit_price", Value::Float(40.0))];
        assert!(e.eval_bool(&ctx(&row)).unwrap());
        let row2 = [("quantity", Value::Int(1)), ("unit_price", Value::Float(40.0))];
        assert!(!e.eval_bool(&ctx(&row2)).unwrap());
    }

    #[test]
    fn logic_and_strings() {
        let e = Expr::parse("state == 'CA' or (vip and age >= 21)").unwrap();
        assert!(e
            .eval_bool(&ctx(&[
                ("state", Value::Text("NY".into())),
                ("vip", Value::Bool(true)),
                ("age", Value::Int(30))
            ]))
            .unwrap());
        assert!(!e
            .eval_bool(&ctx(&[
                ("state", Value::Text("NY".into())),
                ("vip", Value::Bool(false)),
                ("age", Value::Int(30))
            ]))
            .unwrap());
    }

    #[test]
    fn null_semantics() {
        let lk = ctx(&[("x", Value::Null)]);
        assert!(!Expr::parse("x == 5").unwrap().eval_bool(&lk).unwrap());
        assert!(!Expr::parse("x != 5").unwrap().eval_bool(&lk).unwrap());
        assert!(Expr::parse("x is null").unwrap().eval_bool(&lk).unwrap());
        assert!(!Expr::parse("x is not null").unwrap().eval_bool(&lk).unwrap());
        // OR truth-table with unknown
        assert!(Expr::parse("x == 5 or true").unwrap().eval_bool(&lk).unwrap());
        assert!(!Expr::parse("x == 5 and true").unwrap().eval_bool(&lk).unwrap());
    }

    #[test]
    fn in_and_like() {
        let row = [("s", Value::Text("hello world".into())), ("n", Value::Int(2))];
        let lk = ctx(&row);
        assert!(Expr::parse("n in (1, 2, 3)").unwrap().eval_bool(&lk).unwrap());
        assert!(Expr::parse("n not in (4, 5)").unwrap().eval_bool(&lk).unwrap());
        assert!(Expr::parse("s like 'hello%'").unwrap().eval_bool(&lk).unwrap());
        assert!(Expr::parse("s like '%wor__'").unwrap().eval_bool(&lk).unwrap());
        assert!(Expr::parse("s not like 'x%'").unwrap().eval_bool(&lk).unwrap());
    }

    #[test]
    fn precedence() {
        let lk = ctx(&[]);
        // 2 + 3 * 4 == 14
        assert!(Expr::parse("2 + 3 * 4 == 14").unwrap().eval_bool(&lk).unwrap());
        // not binds tighter than and
        assert!(Expr::parse("not false and true").unwrap().eval_bool(&lk).unwrap());
        // unary minus
        assert!(Expr::parse("-2 * -3 == 6").unwrap().eval_bool(&lk).unwrap());
    }

    #[test]
    fn sql_flavor() {
        let lk = ctx(&[("a", Value::Int(1))]);
        assert!(Expr::parse("a = 1").unwrap().eval_bool(&lk).unwrap()); // single =
        assert!(Expr::parse("a <> 2").unwrap().eval_bool(&lk).unwrap()); // <>
        assert!(Expr::parse("'it''s' == 'it''s'").unwrap().eval_bool(&lk).unwrap()); // '' escape
    }

    #[test]
    fn errors() {
        assert!(Expr::parse("a ==").is_err());
        assert!(Expr::parse("(a").is_err());
        assert!(Expr::parse("a like b").is_err());
        let e = Expr::parse("missing == 1").unwrap();
        assert!(e.eval_bool(&|_| None).is_err());
    }

    #[test]
    fn collects_columns() {
        let e = Expr::parse("a + b > c and d like 'x%'").unwrap();
        let mut cols = Vec::new();
        e.columns(&mut cols);
        assert_eq!(cols, vec!["a", "b", "c", "d"]);
    }
}
