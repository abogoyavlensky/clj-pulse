//! Thin typed accessors over `edn_format::Value`, shared by the `deps.edn`
//! and `lgx.edn` readers.

use std::collections::BTreeMap;

use edn_format::{Keyword, Value};

/// A keyword value (`:name`).
pub(crate) fn kw(name: &str) -> Value {
    Value::Keyword(Keyword::from_name(name))
}

/// A namespaced keyword value (`:namespace/name`).
pub(crate) fn kw_ns(namespace: &str, name: &str) -> Value {
    Value::Keyword(Keyword::from_namespace_and_name(namespace, name))
}

/// Looks up `key` in an EDN map.
pub(crate) fn get(map: &BTreeMap<Value, Value>, key: Value) -> Option<&Value> {
    map.get(&key)
}

/// The string behind a `Value::String`, else `None`.
pub(crate) fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// The strings of a `Value::Vector` at `key`. `None` when the key is absent
/// or its value is not a vector; non-string elements are skipped.
pub(crate) fn str_vec_at(map: &BTreeMap<Value, Value>, key: Value) -> Option<Vec<String>> {
    match get(map, key)? {
        Value::Vector(v) => Some(v.iter().filter_map(as_str).map(str::to_string).collect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_vec_at_reads_string_vector() {
        let Value::Map(map) = edn_format::parse_str(r#"{:paths ["a" "b"] :n 1}"#).unwrap() else {
            panic!("expected a map");
        };
        assert_eq!(
            str_vec_at(&map, kw("paths")),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        // Missing key.
        assert_eq!(str_vec_at(&map, kw("missing")), None);
        // Present but not a vector.
        assert_eq!(str_vec_at(&map, kw("n")), None);
    }
}
