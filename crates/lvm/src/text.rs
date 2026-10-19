//! The LVM2 metadata text format (libvslvm "Logical Volume Manager (LVM) format", section 4,
//! "Parameter definitions").
//!
//! Text is a list of items. An item is `name = value` or `name { items }`. A value is an
//! integer, a double-quoted string (with `\"` and `\\` escapes), or a bracketed,
//! comma-separated list of integers and strings. `#` starts a comment that runs to the end of
//! the line, and whitespace separates tokens. A trailing NUL, as the metadata area stores the
//! text, ends it.

use alloc::string::String;
use alloc::vec::Vec;

use thiserror::Error;

/// How deeply sections may nest. Volume-group metadata uses four levels; the bound keeps
/// hostile text from exhausting the stack.
pub const MAX_DEPTH: usize = 16;

/// A parsed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// An integer.
    Int(i64),
    /// A string.
    Str(String),
    /// A list of integers and strings.
    List(Vec<Value>),
    /// A section of named items, in order.
    Section(Section),
}

/// A section: named items in the order they appear.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    /// The items.
    pub items: Vec<(String, Value)>,
}

impl Section {
    /// The first item named `name`.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.items.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// The subsections, in order.
    pub fn sections(&self) -> impl Iterator<Item = (&str, &Section)> {
        self.items.iter().filter_map(|(n, v)| match v {
            Value::Section(s) => Some((n.as_str(), s)),
            _ => None,
        })
    }
}

/// Parses metadata text into its top-level section.
///
/// # Errors
///
/// A [`TextError`] with the byte offset of the first problem.
pub fn parse(text: &[u8]) -> Result<Section, TextError> {
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    let mut parser = Parser {
        text: &text[..end],
        at: 0,
    };
    let section = parser.items(0)?;
    parser.skip_space();
    if parser.at < parser.text.len() {
        return Err(parser.error(TextErrorKind::Unexpected));
    }
    Ok(section)
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn error(&self, kind: TextErrorKind) -> TextError {
        TextError {
            offset: self.at,
            kind,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b'#' => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.at += 1;
                    }
                }
                b if b.is_ascii_whitespace() => self.at += 1,
                _ => break,
            }
        }
    }

    /// Items until a closing brace (left unread) or the end of the text.
    fn items(&mut self, depth: usize) -> Result<Section, TextError> {
        if depth > MAX_DEPTH {
            return Err(self.error(TextErrorKind::TooDeep));
        }
        let mut section = Section::default();
        loop {
            self.skip_space();
            match self.peek() {
                None | Some(b'}') => return Ok(section),
                Some(_) => {}
            }
            let name = self.name()?;
            self.skip_space();
            let value = match self.peek() {
                Some(b'=') => {
                    self.at += 1;
                    self.skip_space();
                    self.value()?
                }
                Some(b'{') => {
                    self.at += 1;
                    let inner = self.items(depth + 1)?;
                    if self.peek() != Some(b'}') {
                        return Err(self.error(TextErrorKind::Unclosed));
                    }
                    self.at += 1;
                    Value::Section(inner)
                }
                _ => return Err(self.error(TextErrorKind::Unexpected)),
            };
            section.items.push((name, value));
        }
    }

    fn name(&mut self) -> Result<String, TextError> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
        {
            self.at += 1;
        }
        if self.at == start {
            return Err(self.error(TextErrorKind::Unexpected));
        }
        Ok(self.text[start..self.at]
            .iter()
            .map(|&b| char::from(b))
            .collect())
    }

    fn value(&mut self) -> Result<Value, TextError> {
        match self.peek() {
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => self.list(),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.int().map(Value::Int),
            _ => Err(self.error(TextErrorKind::Unexpected)),
        }
    }

    fn list(&mut self) -> Result<Value, TextError> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.peek() == Some(b']') {
                self.at += 1;
                return Ok(Value::List(items));
            }
            if !items.is_empty() {
                if self.peek() != Some(b',') {
                    return Err(self.error(TextErrorKind::Unexpected));
                }
                self.at += 1;
                self.skip_space();
            }
            match self.peek() {
                Some(b'"') => items.push(Value::Str(self.string()?)),
                Some(b) if b == b'-' || b.is_ascii_digit() => items.push(Value::Int(self.int()?)),
                None => return Err(self.error(TextErrorKind::Unclosed)),
                _ => return Err(self.error(TextErrorKind::Unexpected)),
            }
        }
    }

    fn int(&mut self) -> Result<i64, TextError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let digits = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        if self.at == digits {
            return Err(self.error(TextErrorKind::Unexpected));
        }
        let text = core::str::from_utf8(&self.text[start..self.at]).expect("ASCII digits");
        text.parse().map_err(|_| TextError {
            offset: start,
            kind: TextErrorKind::IntRange,
        })
    }

    fn string(&mut self) -> Result<String, TextError> {
        let start = self.at;
        self.at += 1;
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                None => {
                    return Err(TextError {
                        offset: start,
                        kind: TextErrorKind::Unclosed,
                    });
                }
                Some(b'"') => {
                    self.at += 1;
                    break;
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escaped = self.peek().ok_or(TextError {
                        offset: start,
                        kind: TextErrorKind::Unclosed,
                    })?;
                    bytes.push(escaped);
                    self.at += 1;
                }
                Some(b) => {
                    bytes.push(b);
                    self.at += 1;
                }
            }
        }
        String::from_utf8(bytes).map_err(|_| TextError {
            offset: start,
            kind: TextErrorKind::Utf8,
        })
    }
}

/// Metadata text that does not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("LVM metadata text is not valid at byte {offset}: {kind}")]
pub struct TextError {
    /// The byte offset of the problem.
    pub offset: usize,
    /// What is wrong.
    pub kind: TextErrorKind,
}

/// The kinds of [`TextError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TextErrorKind {
    /// A character that cannot start or continue what is expected there.
    #[error("unexpected character")]
    Unexpected,
    /// A string, list or section that is not closed.
    #[error("unclosed string, list or section")]
    Unclosed,
    /// An integer outside the 64-bit signed range.
    #[error("integer out of range")]
    IntRange,
    /// A string that is not UTF-8.
    #[error("string is not UTF-8")]
    Utf8,
    /// Sections nested deeper than [`MAX_DEPTH`].
    #[error("sections nested too deeply")]
    TooDeep,
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;

    #[test]
    fn metadata_text_parses() {
        let text = br#"# Generated by LVM2
contents = "Text Format Volume Group"
version = 1
vg {
	id = "aj3Hpd-txx0-B07i-ljJn-0xTE-AT8r-69fGDY"   # comment
	status = ["RESIZEABLE", "READ"]
	flags = []
	lv.name+x {
		stripes = [
			"pv0", 0,
			"pv1", -4
		]
		description = "a \"quoted\" \\ path"
	}
}
"#;
        let root = parse(&[&text[..], b"\0junk after the NUL"].concat()).expect("valid");
        assert_eq!(root.get("version"), Some(&Value::Int(1)));
        let (name, vg) = root.sections().next().expect("one section");
        assert_eq!(name, "vg");
        assert_eq!(
            vg.get("status"),
            Some(&Value::List(vec![
                Value::Str("RESIZEABLE".into()),
                Value::Str("READ".into())
            ]))
        );
        assert_eq!(vg.get("flags"), Some(&Value::List(vec![])));
        let (lv_name, lv) = vg.sections().next().unwrap();
        assert_eq!(lv_name, "lv.name+x");
        assert_eq!(
            lv.get("stripes"),
            Some(&Value::List(vec![
                Value::Str("pv0".into()),
                Value::Int(0),
                Value::Str("pv1".into()),
                Value::Int(-4)
            ]))
        );
        assert_eq!(
            lv.get("description"),
            Some(&Value::Str("a \"quoted\" \\ path".into()))
        );
    }

    #[test]
    fn errors_have_offsets() {
        let cases: [(&[u8], usize, TextErrorKind); 8] = [
            (b"a = ", 4, TextErrorKind::Unexpected),
            (b"a = \"open", 4, TextErrorKind::Unclosed),
            (b"a { b = 1", 9, TextErrorKind::Unclosed),
            (b"a = [1 2]", 7, TextErrorKind::Unexpected),
            (b"a = [1,", 7, TextErrorKind::Unclosed),
            (b"a = 99999999999999999999", 4, TextErrorKind::IntRange),
            (b"a = \"\xff\"", 4, TextErrorKind::Utf8),
            (b"}", 0, TextErrorKind::Unexpected),
        ];
        for (text, offset, kind) in cases {
            assert_eq!(parse(text), Err(TextError { offset, kind }), "{text:?}");
        }
        let deep = "a {".repeat(MAX_DEPTH + 1) + &"}".repeat(MAX_DEPTH + 1);
        assert_eq!(
            parse(deep.as_bytes()).unwrap_err().kind,
            TextErrorKind::TooDeep
        );
        assert_eq!(
            TextError {
                offset: 3,
                kind: TextErrorKind::Utf8
            }
            .to_string(),
            "LVM metadata text is not valid at byte 3: string is not UTF-8"
        );
    }
}
