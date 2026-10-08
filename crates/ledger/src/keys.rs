//! The untyped pass both table loaders run before their typed parse. The typed maps read a key as its text and keep
//! the last of two equal keys, so a tagged key whose text repeats another, or a key that is not a string, would load
//! a row the file does not say.

use std::collections::HashSet;

use serde_norway::Value;

/// Refuses a mapping key, at any depth, that repeats another key of its mapping once read as its text, then one that
/// is not an untagged string, naming the key and the path of its mapping. serde_norway refuses two equal untagged
/// keys itself, with their line and column, so a repeat found here is a tagged key.
pub(crate) fn untagged_string_keys(text: &str) -> Result<(), String> {
    let value: Value = serde_norway::from_str(text).map_err(|e| e.to_string())?;
    walk(&value, "")
}

fn walk(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::Mapping(mapping) => {
            let mut seen = HashSet::new();
            for (key, value) in mapping {
                let at = joined(path, &shown(key));
                if let Some(text) = text_of(key) {
                    if !seen.insert(text) {
                        return Err(format!("{at}: a duplicate of the key {text}"));
                    }
                }
                let Value::String(text) = key else {
                    return Err(format!("{at}: a key is an untagged string"));
                };
                walk(value, &joined(path, text))?;
            }
            Ok(())
        }
        Value::Sequence(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(index, item)| walk(item, &format!("{path}[{index}]"))),
        Value::Tagged(tagged) => walk(&tagged.value, path),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// The text the typed parse reads a key as: a string, tagged or not.
fn text_of(key: &Value) -> Option<&str> {
    match key {
        Value::String(text) => Some(text),
        Value::Tagged(tagged) => match &tagged.value {
            Value::String(text) => Some(text),
            _ => None,
        },
        _ => None,
    }
}

fn shown(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Sequence(items) => {
            let items: Vec<String> = items.iter().map(shown).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Mapping(mapping) => {
            let entries: Vec<String> = mapping
                .iter()
                .map(|(key, value)| format!("{}: {}", shown(key), shown(value)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        Value::Tagged(tagged) => format!("{} {}", tagged.tag, shown(&tagged.value)),
    }
}

fn joined(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}
