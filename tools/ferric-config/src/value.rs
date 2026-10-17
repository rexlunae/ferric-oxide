use std::fmt;

/// A resolved option value. Choices are stored as strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<String>),
}

impl Value {
    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Str(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
        }
    }

    /// Text used for "{value}" substitution and make variables.
    pub fn render(&self) -> String {
        match self {
            Value::Bool(b) => b.to_string(),
            Value::Int(n) => n.to_string(),
            Value::Str(s) => s.clone(),
            Value::List(l) => l.join(" "),
        }
    }

    pub fn from_toml(v: &toml::Value) -> Option<Value> {
        Some(match v {
            toml::Value::Boolean(b) => Value::Bool(*b),
            toml::Value::Integer(n) => Value::Int(*n),
            toml::Value::String(s) => Value::Str(s.clone()),
            toml::Value::Array(items) => Value::List(
                items
                    .iter()
                    .map(|i| i.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?,
            ),
            _ => return None,
        })
    }

    pub fn to_toml(&self) -> toml::Value {
        match self {
            Value::Bool(b) => toml::Value::Boolean(*b),
            Value::Int(n) => toml::Value::Integer(*n),
            Value::Str(s) => toml::Value::String(s.clone()),
            Value::List(l) => {
                toml::Value::Array(l.iter().cloned().map(toml::Value::String).collect())
            }
        }
    }

    pub fn to_edit(&self) -> toml_edit::Value {
        match self {
            Value::Bool(b) => (*b).into(),
            Value::Int(n) => (*n).into(),
            Value::Str(s) => s.as_str().into(),
            Value::List(l) => {
                let mut a = toml_edit::Array::new();
                for s in l {
                    a.push(s.as_str());
                }
                toml_edit::Value::Array(a)
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Bool(b) => (*b).into(),
            Value::Int(n) => (*n).into(),
            Value::Str(s) => s.as_str().into(),
            Value::List(l) => l.clone().into(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(true) => write!(f, "y"),
            Value::Bool(false) => write!(f, "n"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::List(l) => write!(f, "{l:?}"),
        }
    }
}
