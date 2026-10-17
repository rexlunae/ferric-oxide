//! The small expression language used by `depends`, `defaults[].when`,
//! `effect[].when` and `rule[].when` in features.toml.
//!
//! ```text
//! expr    := and ("||" and)*
//! and     := unary ("&&" unary)*
//! unary   := "!" unary | "(" expr ")" | compare
//! compare := operand (("==" | "!=" | "<" | "<=" | ">" | ">=" | "in") operand)?
//! operand := ident | 'str' | "str" | int | true | false | "[" operand,* "]"
//! ```
//!
//! A bare identifier is truthy when it is `true`, a non-empty string or
//! list, or a non-zero integer.

use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(Box<Expr>, CmpOp, Box<Expr>),
    Ident(String),
    Lit(Value),
    List(Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Op(&'static str),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            let quote = c;
            let start = i + 1;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            if i >= chars.len() {
                return Err(format!("unterminated string in `{src}`"));
            }
            toks.push(Tok::Str(chars[start..i].iter().collect()));
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            toks.push(Tok::Int(s.parse().map_err(|e| format!("{s}: {e}"))?));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '_' | '.' | '-'))
            {
                i += 1;
            }
            toks.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let op = match two.as_str() {
            "==" => Some("=="),
            "!=" => Some("!="),
            "<=" => Some("<="),
            ">=" => Some(">="),
            "&&" => Some("&&"),
            "||" => Some("||"),
            _ => None,
        };
        if let Some(op) = op {
            toks.push(Tok::Op(op));
            i += 2;
            continue;
        }
        let op = match c {
            '!' => "!",
            '<' => "<",
            '>' => ">",
            '(' => "(",
            ')' => ")",
            '[' => "[",
            ']' => "]",
            ',' => ",",
            _ => return Err(format!("unexpected `{c}` in `{src}`")),
        };
        toks.push(Tok::Op(op));
        i += 1;
    }
    Ok(toks)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    src: &'a str,
}

impl Parser<'_> {
    fn peek_op(&self, op: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(Tok::Op(o)) if *o == op)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.peek_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(format!("expected `{op}` in `{}`", self.src))
        }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.and()?;
        while self.eat_op("||") {
            lhs = Expr::Or(Box::new(lhs), Box::new(self.and()?));
        }
        Ok(lhs)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while self.eat_op("&&") {
            lhs = Expr::And(Box::new(lhs), Box::new(self.unary()?));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat_op("!") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat_op("(") {
            let e = self.or()?;
            self.expect_op(")")?;
            return Ok(e);
        }
        self.compare()
    }

    fn compare(&mut self) -> Result<Expr, String> {
        let lhs = self.operand()?;
        let op = match self.toks.get(self.pos) {
            Some(Tok::Op("==")) => CmpOp::Eq,
            Some(Tok::Op("!=")) => CmpOp::Ne,
            Some(Tok::Op("<")) => CmpOp::Lt,
            Some(Tok::Op("<=")) => CmpOp::Le,
            Some(Tok::Op(">")) => CmpOp::Gt,
            Some(Tok::Op(">=")) => CmpOp::Ge,
            Some(Tok::Ident(s)) if s == "in" => CmpOp::In,
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.operand()?;
        Ok(Expr::Cmp(Box::new(lhs), op, Box::new(rhs)))
    }

    fn operand(&mut self) -> Result<Expr, String> {
        let tok = self
            .toks
            .get(self.pos)
            .cloned()
            .ok_or_else(|| format!("unexpected end of `{}`", self.src))?;
        self.pos += 1;
        match tok {
            Tok::Str(s) => Ok(Expr::Lit(Value::Str(s))),
            Tok::Int(n) => Ok(Expr::Lit(Value::Int(n))),
            Tok::Ident(s) if s == "true" => Ok(Expr::Lit(Value::Bool(true))),
            Tok::Ident(s) if s == "false" => Ok(Expr::Lit(Value::Bool(false))),
            Tok::Ident(s) => Ok(Expr::Ident(s)),
            Tok::Op("[") => {
                let mut items = Vec::new();
                if !self.eat_op("]") {
                    loop {
                        items.push(self.operand()?);
                        if self.eat_op("]") {
                            break;
                        }
                        self.expect_op(",")?;
                    }
                }
                Ok(Expr::List(items))
            }
            Tok::Op(o) => Err(format!("unexpected `{o}` in `{}`", self.src)),
        }
    }
}

pub fn parse(src: &str) -> Result<Expr, String> {
    let mut p = Parser {
        toks: tokenize(src)?,
        pos: 0,
        src,
    };
    let e = p.or()?;
    if p.pos != p.toks.len() {
        return Err(format!("trailing input in `{src}`"));
    }
    Ok(e)
}

impl Expr {
    /// Every identifier the expression reads, for schema validation.
    pub fn idents(&self, out: &mut Vec<String>) {
        match self {
            Expr::Or(a, b) | Expr::And(a, b) | Expr::Cmp(a, _, b) => {
                a.idents(out);
                b.idents(out);
            }
            Expr::Not(a) => a.idents(out),
            Expr::Ident(s) => out.push(s.clone()),
            Expr::List(items) => items.iter().for_each(|i| i.idents(out)),
            Expr::Lit(_) => {}
        }
    }

    fn value(&self, lookup: &dyn Fn(&str) -> Option<Value>) -> Result<Value, String> {
        match self {
            Expr::Ident(s) => lookup(s).ok_or_else(|| format!("unknown option `{s}`")),
            Expr::Lit(v) => Ok(v.clone()),
            Expr::List(items) => {
                let mut out = Vec::new();
                for i in items {
                    match i.value(lookup)? {
                        Value::Str(s) => out.push(s),
                        other => out.push(other.render()),
                    }
                }
                Ok(Value::List(out))
            }
            other => Ok(Value::Bool(other.eval(lookup)?)),
        }
    }

    pub fn eval(&self, lookup: &dyn Fn(&str) -> Option<Value>) -> Result<bool, String> {
        match self {
            Expr::Or(a, b) => Ok(a.eval(lookup)? || b.eval(lookup)?),
            Expr::And(a, b) => Ok(a.eval(lookup)? && b.eval(lookup)?),
            Expr::Not(a) => Ok(!a.eval(lookup)?),
            Expr::Cmp(a, op, b) => {
                let (a, b) = (a.value(lookup)?, b.value(lookup)?);
                match op {
                    CmpOp::Eq => Ok(a == b),
                    CmpOp::Ne => Ok(a != b),
                    CmpOp::In => match &b {
                        Value::List(items) => Ok(items.contains(&a.render())),
                        _ => Err("right side of `in` must be a list".into()),
                    },
                    _ => match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Ok(match op {
                            CmpOp::Lt => x < y,
                            CmpOp::Le => x <= y,
                            CmpOp::Gt => x > y,
                            _ => x >= y,
                        }),
                        _ => Err(format!("ordering needs two integers, got {a:?} and {b:?}")),
                    },
                }
            }
            e => Ok(e.value(lookup)?.truthy()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> bool {
        let lookup = |name: &str| match name {
            "a" => Some(Value::Bool(true)),
            "b" => Some(Value::Bool(false)),
            "cpu.level" => Some(Value::Str("v3".into())),
            "size" => Some(Value::Int(10)),
            "empty" => Some(Value::Str(String::new())),
            _ => None,
        };
        parse(src).unwrap().eval(&lookup).unwrap()
    }

    #[test]
    fn logic() {
        assert!(eval("a"));
        assert!(!eval("b"));
        assert!(eval("a && !b"));
        assert!(eval("b || a"));
        assert!(!eval("!(a || b)"));
        assert!(!eval("empty"));
    }

    #[test]
    fn comparisons() {
        assert!(eval("cpu.level == 'v3'"));
        assert!(eval("cpu.level != \"v1\""));
        assert!(eval("cpu.level in ['v3', 'v4']"));
        assert!(!eval("cpu.level in []"));
        assert!(eval("size > 0 && size <= 10"));
        assert!(eval("size >= -1"));
        assert!(eval("a == true"));
    }

    #[test]
    fn errors() {
        assert!(parse("a &&").is_err());
        assert!(parse("'open").is_err());
        assert!(parse("a b").is_err());
        let lookup = |_: &str| None;
        assert!(parse("missing").unwrap().eval(&lookup).is_err());
    }
}
