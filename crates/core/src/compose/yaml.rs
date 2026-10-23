//! A minimal YAML value tree.
//!
//! Validation needs the document as written, in file order, with every key
//! visible (so unknown ones can be reported with their path). This tree
//! keeps exactly that, and rejects duplicate keys at parse time.

use core::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A YAML value. Numbers keep their value only when it is a non-negative
/// integer (a port in `expose`); validation never looks at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Value {
    Null,
    Bool(bool),
    Number(Option<u64>),
    String(String),
    Sequence(Vec<Self>),
    Mapping(Mapping),
}

/// A mapping in file order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Mapping {
    entries: Vec<(Value, Value)>,
}

impl Mapping {
    pub(super) const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&Value, &Value)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(k, _)| k)
    }

    pub(super) fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    }

    pub(super) fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

impl<'a> IntoIterator for &'a Mapping {
    type Item = (&'a Value, &'a Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a Value, &'a Value)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl Value {
    pub(super) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value of a non-negative integer.
    pub(super) const fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(n) => *n,
            _ => None,
        }
    }

    pub(super) const fn as_mapping(&self) -> Option<&Mapping> {
        match self {
            Self::Mapping(m) => Some(m),
            _ => None,
        }
    }

    pub(super) fn as_sequence(&self) -> Option<&[Self]> {
        match self {
            Self::Sequence(s) => Some(s),
            _ => None,
        }
    }

    pub(super) const fn is_mapping(&self) -> bool {
        matches!(self, Self::Mapping(_))
    }

    pub(super) const fn is_sequence(&self) -> bool {
        matches!(self, Self::Sequence(_))
    }

    /// `mapping[key]` for a mapping, `None` otherwise.
    pub(super) fn get(&self, key: &str) -> Option<&Self> {
        self.as_mapping()?.get(key)
    }
}

/// Parses `text` into a value tree.
pub(super) fn parse(text: &str) -> Result<Value, String> {
    serde_saphyr::from_str::<Value>(text).map_err(|e| e.to_string())
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any YAML value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        Value::deserialize(deserializer)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(u64::try_from(v).ok()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(Some(v)))
    }

    fn visit_i128<E: de::Error>(self, v: i128) -> Result<Value, E> {
        Ok(Value::Number(u64::try_from(v).ok()))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Value, E> {
        Ok(Value::Number(u64::try_from(v).ok()))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Value, E> {
        Ok(Value::Number(None))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Sequence(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<Value, Value>()? {
            if entries.iter().any(|(seen, _)| *seen == key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            entries.push((key, value));
        }
        Ok(Value::Mapping(Mapping { entries }))
    }
}
