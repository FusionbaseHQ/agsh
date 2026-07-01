use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    Path(PathBuf),
    List(Vec<Value>),
    Record(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_string_lossy(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(v) => v.to_string(),
            Value::Int(v) => v.to_string(),
            Value::Float(v) => v.to_string(),
            Value::String(v) => v.clone(),
            Value::Bytes(v) => String::from_utf8_lossy(v).to_string(),
            Value::Path(v) => v.display().to_string(),
            Value::List(values) => values
                .iter()
                .map(Value::as_string_lossy)
                .collect::<Vec<_>>()
                .join(" "),
            Value::Record(_) => "<record>".to_string(),
        }
    }
}
